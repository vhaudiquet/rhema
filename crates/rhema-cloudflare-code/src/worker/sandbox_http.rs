//! The host-fetch transport under the `http` sandbox backend: the
//! [`HttpTransport`] the DO hands [`HttpSandbox`] when the deployment
//! selects a protocol daemon over the shim.
#![forbid(unsafe_code)]

use async_trait::async_trait;
use worker::{Fetch, Headers, Method, Request, RequestInit};

use rhema_sandbox::SandboxError;
use rhema_sandbox::http::HttpTransport;
use rhema_sandbox::protocol::TOKEN_HEADER;

/// [`HttpTransport`] over workerd's host fetch. Stateless: workerd owns
/// the socket and the connection pool. The exchange runs inside one
/// local task — the fetch and its body text are JS-backed and `!Send`,
/// so no JS handle may cross an await the seam's future owns — and only
/// the plain status-and-bytes pair crosses back, `SandboxDo::post`'s
/// spawn_local bridge.
#[derive(Debug, Clone, Copy, Default)]
pub struct FetchHttpTransport;

#[async_trait]
impl HttpTransport for FetchHttpTransport {
    async fn post(
        &self,
        url: &str,
        token: &str,
        body: Vec<u8>,
    ) -> Result<(u16, Vec<u8>), SandboxError> {
        let mut init = RequestInit::new();
        init.method = Method::Post;
        init.headers = Headers::new();
        init.headers
            .set(TOKEN_HEADER, token)
            .map_err(|err| SandboxError::Unavailable(err.to_string()))?;
        init.headers
            .set("content-type", "application/json")
            .map_err(|err| SandboxError::Unavailable(err.to_string()))?;
        // A body rides as bytes; the JS side wants a typed array.
        init.body = Some(worker::js_sys::Uint8Array::from(body.as_slice()).into());
        let request = Request::new_with_init(url, &init)
            .map_err(|err| SandboxError::Unavailable(err.to_string()))?;
        let (status_slot, status) = futures::channel::oneshot::channel();
        let (body_slot, body) = futures::channel::oneshot::channel::<String>();
        wasm_bindgen_futures::spawn_local(async move {
            let outcome: worker::Result<(u16, String)> = async {
                let mut response = Fetch::Request(request).send().await?;
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
                    // Status 0 marks "no HTTP answer": a real reply
                    // always carries its code, so the pair stays
                    // unambiguous.
                    let _ = status_slot.send(0);
                    let _ = body_slot.send(err.to_string());
                }
            }
        });
        let status = status.await.map_err(|_| {
            SandboxError::Unavailable("the fetch task ended without a status".into())
        })?;
        let text = body
            .await
            .map_err(|_| SandboxError::Unavailable("the fetch task ended without a body".into()))?;
        if status == 0 {
            // The fetch itself failed; the body slot carries workerd's
            // error text.
            return Err(SandboxError::Unavailable(text));
        }
        Ok((status, text.into_bytes()))
    }
}
