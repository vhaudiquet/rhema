//! The [`LlamaCppProvider`]: HTTP and SSE transport for llama.cpp's
//! `llama-server` OpenAI-compatible API.
//!
//! One `POST {base}/v1/chat/completions` with `stream: true` per exchange;
//! the optional API key rides in the `Authorization: Bearer` header — never
//! a query parameter, so it never lands in URLs or access logs. Transport
//! failures map onto [`ProviderError`]: timeouts to
//! [`ProviderError::Timeout`], other network failures to
//! [`ProviderError::Network`], non-2xx responses (with the body's
//! `error.message` when present) to [`ProviderError::Api`] with the HTTP
//! status, and unparseable stream chunks and mid-stream error chunks to
//! [`ProviderError::Api`] without one.

use async_trait::async_trait;
use rhema_core::event::EventSink;
use rhema_core::output::ModelOutput;
use rhema_core::provider::{ModelProvider, ProviderError};
use rhema_core::request::ModelRequest;
use rhema_provider::ModelCard;
use rhema_provider_openai_compatible::provider::{
    OpenAiCompatibleProvider, network_error as dialect_network_error,
};

use crate::wire::LLAMA_CPP;
use rhema_provider_openai_compatible::wire::WireErrorBody;

/// The `llama-server` default listen address.
pub const DEFAULT_BASE_URL: &str = "http://localhost:8080";

/// llama.cpp `llama-server` backend, speaking its OpenAI-compatible
/// chat-completions API through the shared dialect core.
pub struct LlamaCppProvider {
    base_url: String,
    api_key: Option<String>,
    http: reqwest::Client,
    /// The dialect-core transport carrying llama.cpp's dialect; every
    /// completion goes through it.
    transport: OpenAiCompatibleProvider,
}

impl std::fmt::Debug for LlamaCppProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The key never prints; the plain data does.
        f.debug_struct("LlamaCppProvider")
            .field("base_url", &self.base_url)
            .field("api_key", &"<redacted>")
            .field("http", &self.http)
            .finish()
    }
}

impl LlamaCppProvider {
    /// A provider against `base_url` (trailing slashes trimmed) with no
    /// authentication — the default `llama-server` setup.
    pub fn new(base_url: impl Into<String>) -> Self {
        let base_url = base_url.into().trim_end_matches('/').to_owned();
        Self {
            transport: transport_for(&base_url, None),
            api_key: None,
            base_url,
            // A dead server must fail fast: connects cap at 10s, and a
            // stream that stops producing bytes for 90s is dead too.
            // Streaming calls get no total timeout — generations can run
            // for minutes.
            http: reqwest::Client::builder()
                .connect_timeout(std::time::Duration::from_secs(10))
                .read_timeout(std::time::Duration::from_secs(90))
                .build()
                .expect("a client builder with only timeouts cannot fail"),
        }
    }

    /// Authenticates with an `Authorization: Bearer` header, for servers
    /// started with `--api-key`. Header only: the key never goes in a URL.
    pub fn with_api_key(mut self, key: impl Into<String>) -> Self {
        self.api_key = Some(key.into());
        self.transport = transport_for(&self.base_url, self.api_key.as_deref());
        self
    }

    /// The models the server can serve, normalized into the shared
    /// [`ModelCard`] rows (`GET /v1/models`; llama.cpp lists the one
    /// loaded model). A single-model server also gets its context from
    /// `/props`; in router mode, models that are loaded and still silent
    /// about their context are asked per model (bounded — never for
    /// unloaded entries, which some servers would autoload just to
    /// answer).
    ///
    /// # Errors
    ///
    /// Fails with [`ProviderError::Network`]/[`ProviderError::Timeout`]
    /// when the transport fails, [`ProviderError::Api`] with the HTTP
    /// status on a non-2xx listing, and [`ProviderError::Api`] without
    /// one on a malformed listing or props body.
    pub async fn list_models(&self) -> Result<Vec<ModelCard>, ProviderError> {
        let response = self
            .http
            .get(format!("{}/v1/models", self.base_url))
            .authorized(&self.api_key)
            .timeout(std::time::Duration::from_secs(20))
            .send()
            .await
            .map_err(network_error)?;
        let status = response.status();
        if !status.is_success() {
            let text = response.text().await.unwrap_or_default();
            return Err(api_error(status, &text));
        }
        let page: serde_json::Value = response.json().await.map_err(|err| ProviderError::Api {
            message: format!("malformed models page: {err}"),
            status: None,
        })?;
        let mut models = crate::wire::normalize_models(&page);

        if models.len() == 1 && models[0].context_length.is_none() {
            if let Some(n_ctx) = self.props(None).await? {
                models[0].context_length = Some(n_ctx);
            }
        } else {
            let silent: Vec<String> = models
                .iter()
                .filter(|model| model.loaded == Some(true) && model.context_length.is_none())
                .take(8)
                .map(|model| model.id.clone())
                .collect();
            for id in silent {
                if let Some(n_ctx) = self.props(Some(&id)).await?
                    && let Some(model) = models.iter_mut().find(|model| model.id == id)
                {
                    model.context_length = Some(n_ctx);
                }
            }
        }
        Ok(models.into_iter().map(ModelCard::from).collect())
    }

    /// The server's runtime properties, optionally for one model. `n_ctx`
    /// sits at the top level on single-model servers; the router proxies
    /// per-model queries to the instance.
    async fn props(&self, model: Option<&str>) -> Result<Option<u64>, ProviderError> {
        let mut url = format!("{}/props", self.base_url);
        if let Some(model) = model {
            url.push_str(&format!("?model={}", urlencode(model)));
        }
        let response = self
            .http
            .get(url)
            .authorized(&self.api_key)
            .timeout(std::time::Duration::from_secs(20))
            .send()
            .await
            .map_err(network_error)?;
        let status = response.status();
        if !status.is_success() {
            return Ok(None);
        }
        let props: serde_json::Value = response.json().await.map_err(|err| ProviderError::Api {
            message: format!("malformed props: {err}"),
            status: None,
        })?;
        Ok(crate::wire::context_from_props(&props))
    }
}

/// Percent-encodes a model id for a query string. Ids are names, but
/// routers have been seen serving ids with slashes and spaces.
fn urlencode(model: &str) -> String {
    let mut encoded = String::new();
    for byte in model.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                encoded.push(byte as char)
            }
            _ => encoded.push_str(&format!("%{byte:02X}")),
        }
    }
    encoded
}

/// Attaches the bearer header when a key is configured.
trait Authorized {
    fn authorized(self, api_key: &Option<String>) -> reqwest::RequestBuilder;
}

impl Authorized for reqwest::RequestBuilder {
    fn authorized(self, api_key: &Option<String>) -> reqwest::RequestBuilder {
        match api_key {
            Some(key) => self.bearer_auth(key),
            None => self,
        }
    }
}

/// Maps a non-2xx body onto [`ProviderError::Api`], keeping the OpenAI
/// error shape's `error.message` when the body has it, and carrying the
/// HTTP status structurally for [`ProviderError::is_retryable`].
fn api_error(status: reqwest::StatusCode, raw: &str) -> ProviderError {
    let parsed: WireErrorBody = serde_json::from_str(raw).unwrap_or_default();
    ProviderError::Api {
        // The listing-side prefix names the server, not the dialect.
        message: format!(
            "llama.cpp api error (HTTP {}): {}",
            status.as_u16(),
            parsed.message(raw)
        ),
        status: Some(status.as_u16()),
    }
}

/// Maps a transport failure onto [`ProviderError`]; reqwest errors cannot be
/// carried in [`ProviderError`] directly (it is `Clone + Eq`).
fn network_error(err: reqwest::Error) -> ProviderError {
    dialect_network_error(err)
}

/// The dialect-core transport for one llama.cpp server: its `/v1`
/// base, its dialect, its optional bearer key.
fn transport_for(base_url: &str, api_key: Option<&str>) -> OpenAiCompatibleProvider {
    OpenAiCompatibleProvider::new(api_key.unwrap_or_default())
        .with_base_url(format!("{base_url}/v1"))
        .with_dialect(LLAMA_CPP)
}

#[async_trait]
impl ModelProvider for LlamaCppProvider {
    /// # Errors
    ///
    /// Fails exactly as the shared dialect core completes: transport
    /// failures map to [`ProviderError::Network`]/[`ProviderError::Timeout`],
    /// non-2xx responses (with the body's `error.message` when present)
    /// to [`ProviderError::Api`] with the HTTP status, and malformed
    /// stream chunks or mid-stream error chunks to [`ProviderError::Api`]
    /// without one.
    async fn complete(
        &self,
        req: ModelRequest,
        sink: &mut dyn EventSink,
    ) -> Result<ModelOutput, ProviderError> {
        self.transport.complete(req, sink).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_prints_presence_never_the_key() {
        let provider =
            LlamaCppProvider::new("http://localhost:8080").with_api_key("sk-local-secret");
        let printed = format!("{provider:?}");
        assert!(!printed.contains("sk-local-secret"), "{printed}");
        assert!(printed.contains("<redacted>"), "{printed}");
        // The plain data stays visible.
        assert!(printed.contains("base_url"), "{printed}");
    }
}
