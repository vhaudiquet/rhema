//! The provider: Console's hosted search route over reqwest, feature
//! `native`.

use async_trait::async_trait;
use reqwest::Client;
use std::time::Duration;

use rhema_websearch::{SearchError, SearchQuery, SearchResult, WebSearchProvider};

use crate::wire::{
    OpenCodeSearchRequest, OpenCodeSearchResponse, PROVIDER_ID, results_from_response,
};

/// The Console root: the hosted search route hangs off it.
pub const DEFAULT_BASE_URL: &str = "https://opencode.ai/console";
/// The one route.
pub const WEBSEARCH_PATH: &str = "/api/websearch";
/// The user agent the provider sends, the house spelling. Console
/// asks clients to identify themselves instead of the HTTP library.
pub const USER_AGENT: &str = concat!("rhema/", env!("CARGO_PKG_VERSION"));

/// OpenCode hosted search behind the [`WebSearchProvider`] seam. One
/// client per provider, built in the constructor: reqwest pools
/// connections, and the chain holds providers for the process's life.
/// No request timeout here, the chain's budgets own the clock
/// (`specs/rhema-websearch.md` §5); the client keeps only the connect
/// timeout, so a dead network fails fast instead of hanging the
/// attempt's slot.
pub struct OpenCodeProvider {
    api_key: String,
    base_url: String,
    http: Client,
}

impl OpenCodeProvider {
    /// A provider authenticating `api_key` against
    /// [`DEFAULT_BASE_URL`].
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            api_key: api_key.into(),
            base_url: DEFAULT_BASE_URL.to_owned(),
            http: Client::builder()
                .connect_timeout(Duration::from_secs(10))
                .user_agent(USER_AGENT)
                .build()
                .expect("a client builder with only timeouts cannot fail"),
        }
    }

    /// Overrides the base URL, for tests against a mock server. The
    /// spelling is the Console root (no route), trailing slashes
    /// trimmed.
    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into().trim_end_matches('/').to_owned();
        self
    }
}

#[async_trait]
impl WebSearchProvider for OpenCodeProvider {
    fn kind(&self) -> &'static str {
        "opencode"
    }

    async fn search(&self, query: &SearchQuery) -> Result<Vec<SearchResult>, SearchError> {
        // The hosted route has no count or news field: it answers up
        // to eight general-web results. The caller's count trims the
        // list; `news` cannot narrow a route that has no news index.
        let response = self
            .http
            .post(format!("{}{WEBSEARCH_PATH}", self.base_url))
            .bearer_auth(&self.api_key)
            .json(&OpenCodeSearchRequest {
                query: &query.text,
                provider_id: PROVIDER_ID,
            })
            .send()
            .await
            .map_err(|err| {
                if err.is_timeout() {
                    SearchError::Timeout
                } else {
                    SearchError::Transport(err.to_string())
                }
            })?;
        let status = response.status().as_u16();
        if !response.status().is_success() {
            let head = response.text().await.unwrap_or_default();
            return Err(SearchError::from_status(status, &head));
        }
        let body: OpenCodeSearchResponse = response
            .json()
            .await
            .map_err(|err| SearchError::Parse(err.to_string()))?;
        // The gateway names the provider that ran the search; a
        // different one means the request reached the wrong route, and
        // silence would report another provider's results as this
        // kind's.
        if !body.provider_id.is_empty() && body.provider_id != PROVIDER_ID {
            return Err(SearchError::Parse(format!(
                "the gateway answered as provider {:?}, not {:?}",
                body.provider_id, PROVIDER_ID
            )));
        }
        let mut results = results_from_response(&body);
        results.truncate(usize::from(query.max_results));
        Ok(results)
    }
}
