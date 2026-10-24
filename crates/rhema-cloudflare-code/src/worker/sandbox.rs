//! The sandbox clients of one session and the deployment's choice
//! between them: [`SandboxDo`] over the session's SandboxShim Durable
//! Object, and — under `SANDBOX_BACKEND=http` — the shared
//! [`HttpSandbox`] over host fetch.
//!
//! The shim (a small JS DO, `deploy/cloudflare/sandbox-shim.mjs`) owns
//! the container binding — the container runtime is a JS-only API — and
//! speaks plain JSON over its DO fetch: exec, seed, get, put. The two
//! DOs of one session share a name, so binding by the session's id
//! reaches the container whose lifetime matches the session's
//! checkpoints. A shared secret guards the shim's HTTP surface. Those
//! routes and shapes are the sandbox wire protocol, pinned in
//! `rhema-sandbox::protocol`; the `http` backend speaks the same
//! protocol to a `rhema-sandboxd` on any machine, where the DO binding
//! is never touched at all.
#![forbid(unsafe_code)]

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use async_trait::async_trait;
use base64::Engine;
use worker::{Headers, Method, ObjectNamespace, Request, RequestInit};

use rhema_cloud_agent::{ExecOutcome, ExecRequest, Sandbox, SandboxError, WorkspaceSeed};
use rhema_sandbox::http::HttpSandbox;
use rhema_sandbox::protocol::{
    Ack, EXEC, ExecCall, ExecReply, FileReply, GET, PUT, PutFile, SEED, SeedRequest, TOKEN_HEADER,
    stream_reply,
};

use super::sandbox_http::FetchHttpTransport;

/// The sandbox a turn's `bash` runs in: the deployment's backend choice
/// ([`crate::backend::select_backend`]), resolved once per connection
/// from its env and threaded to every turn. Under
/// [`SandboxSource::Cloudflare`] the shim's DO binding is the only
/// sandbox thing touched; under [`SandboxSource::Http`] no namespace
/// call happens at all — that is the point of the backend.
#[derive(Clone)]
pub(crate) enum SandboxSource {
    /// The session's SandboxShim DO binding.
    Cloudflare(ObjectNamespace),
    /// The protocol daemon's base URL; the session name rides the path.
    Http(String),
}

impl SandboxSource {
    /// The session's sandbox client: the DO wrapper, or the shared HTTP
    /// client over host fetch — the one seam either way, so the turn's
    /// shell and seeding cannot tell the backends apart.
    pub(crate) fn for_session(&self, name: &str, token: &str) -> Arc<dyn Sandbox> {
        match self {
            SandboxSource::Cloudflare(namespace) => {
                Arc::new(SandboxDo::new(namespace.clone(), name, token))
            }
            SandboxSource::Http(base) => Arc::new(HttpSandbox::new_with_transport(
                base.clone(),
                name,
                token,
                Arc::new(FetchHttpTransport),
            )),
        }
    }
}

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
        body: &impl serde::Serialize,
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
            .set(TOKEN_HEADER, &self.token)
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

/// The ack's `error` string as a sandbox result: absent and null both
/// mean success.
fn ack_result(ack: Ack) -> Result<(), SandboxError> {
    match ack.error {
        Some(message) => Err(SandboxError::Failed(message)),
        None => Ok(()),
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
        let call = ExecCall {
            command: request.command,
            cwd: Some(request.cwd),
            timeout_secs: request.timeout.as_secs(),
        };
        let reply: ExecReply = self.post(EXEC, &call).await?;
        // No cancel check here: the command completed remotely and its
        // result is real — the model sees the truth, whatever the turn
        // decided meanwhile.
        if reply.timed_out {
            return Err(SandboxError::Timeout(request.timeout.as_secs()));
        }
        stream_reply(&reply, progress);
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
        ack_result(self.post(SEED, &body).await?)
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
        ack_result(self.post(PUT, &put).await?)
    }
}
