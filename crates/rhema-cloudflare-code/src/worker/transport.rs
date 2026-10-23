//! The host-fetch transport: the [`Transport`] the DO speaks the
//! Anthropic Messages API through, and the bridge that keeps the seam
//! `Send`.
//!
//! workerd gives wasm no sockets; every request rides the host's
//! `fetch`. The response body arrives as a JS-backed stream — raw
//! handles into the JS heap, `!Send` by soundness — so the bridge runs
//! a local task that pumps plain `Vec<u8>` chunks into a channel whose
//! receiver is plain data and therefore `Send`. The JS handle never
//! leaves the bridge; the provider adapters above the seam never know.
#![forbid(unsafe_code)]

use async_trait::async_trait;
use futures::{SinkExt, StreamExt};
use rhema_provider::{HttpRequest, HttpResponse, Transport, TransportError};
use worker::{Fetch, Method, Request, RequestInit};

/// The global-`fetch` transport. Stateless: workerd owns the socket and
/// the connection pool.
#[derive(Debug, Clone, Copy, Default)]
pub struct FetchTransport;

impl FetchTransport {
    /// The transport.
    pub fn new() -> Self {
        Self
    }
}

/// The HTTP method of a request, by name — the seam carries the method
/// as text.
fn method(name: &str) -> Method {
    match name {
        "POST" => Method::Post,
        "PUT" => Method::Put,
        "DELETE" => Method::Delete,
        "HEAD" => Method::Head,
        "PATCH" => Method::Patch,
        _ => Method::Get,
    }
}

#[async_trait]
impl Transport for FetchTransport {
    async fn send(&self, request: HttpRequest) -> Result<HttpResponse, TransportError> {
        let mut init = RequestInit::new();
        init.method = method(&request.method);
        init.headers = worker::Headers::new();
        for (name, value) in &request.headers {
            init.headers
                .set(name, value)
                .map_err(|err| TransportError::Other(err.to_string()))?;
        }
        if let Some(body) = &request.body {
            // A body rides as bytes; the JS side wants a typed array.
            init.body = Some(worker::js_sys::Uint8Array::from(body.as_slice()).into());
        }
        let fetch_request = Request::new_with_init(&request.url, &init)
            .map_err(|err| TransportError::Other(err.to_string()))?;
        // `HttpRequest::timeout` is not carried: workerd gives wasm no
        // Rust-side timers to enforce it with, and the cloud path sends
        // none — a model generation runs unbounded, and workerd bounds
        // the fetch itself.
        //
        // The whole exchange runs inside one local task — the fetch
        // future and the body stream are JS-backed and `!Send` — which
        // reports the status up front and pumps plain chunks into a
        // channel the seam's receiver can own.
        let (chunks, receiver) = futures::channel::mpsc::unbounded();
        let (head_slot, head) = futures::channel::oneshot::channel();
        wasm_bindgen_futures::spawn_local(async move {
            let mut sink = chunks;
            let failure = |err: String| TransportError::Other(err);
            let mut response = match Fetch::Request(fetch_request).send().await {
                Ok(response) => response,
                Err(err) => {
                    let _ = head_slot.send(None);
                    let _ = sink.send(Err(failure(err.to_string()))).await;
                    return;
                }
            };
            let headers: Vec<(String, String)> = response.headers().iter().collect();
            let _ = head_slot.send(Some((response.status_code(), headers)));
            let mut body = match response.stream() {
                Ok(body) => body,
                Err(err) => {
                    let _ = sink.send(Err(failure(err.to_string()))).await;
                    return;
                }
            };
            while let Some(item) = body.next().await {
                let chunk = item.map_err(|err| failure(err.to_string()));
                if sink.send(chunk).await.is_err() {
                    break; // the reader is gone; stop pumping
                }
            }
        });
        let (status, headers) = head
            .await
            .map_err(|_| TransportError::Other("the fetch task ended before its status".into()))?
            .ok_or_else(|| {
                TransportError::Other("the fetch failed before a response head".into())
            })?;
        Ok(HttpResponse {
            status,
            headers,
            body: Box::new(receiver),
        })
    }
}
