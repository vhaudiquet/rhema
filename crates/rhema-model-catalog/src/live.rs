//! The live models.dev fetch, its on-disk cache, and the TTL that
//! decides when to ask again.

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use crate::Catalog;

/// The published endpoint. models.dev serves the whole catalog as one
/// JSON document; there is no per-model query.
pub const DEFAULT_URL: &str = "https://models.dev/api.json";

/// How long a fetched catalog is trusted before another conditional
/// GET. models.dev changes prices and releases a few times a month, so
/// six hours is fresh enough and keeps the 6.9 MB body off most
/// picker opens.
const CACHE_TTL: Duration = Duration::from_secs(6 * 60 * 60);

/// How long a failed fetch suppresses the next attempt. A flaky
/// network must not stall every picker open for the fetch timeout.
const RETRY_AFTER: Duration = Duration::from_secs(5 * 60);

/// The first fetch's budget. The app gives each provider listing 30
/// seconds; the catalog must not eat that whole budget, and a failed
/// fetch leaves the listing without facts, never failed.
const FETCH_TIMEOUT: Duration = Duration::from_secs(8);

/// The user agent models.dev sees.
const USER_AGENT: &str = concat!("rhema/", env!("CARGO_PKG_VERSION"));

/// One fetch result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fetched {
    /// The server confirmed the cached body is current.
    NotModified,
    /// A new body and the validator for the next request.
    Fresh {
        /// The raw `api.json`.
        body: String,
        /// The `ETag` the next request sends as `If-None-Match`.
        etag: Option<String>,
    },
}

/// Where the catalog comes from. The default is [`ModelsDev`]; the
/// seam exists so tests can answer without a socket.
#[async_trait]
pub trait Source: Send + Sync {
    /// Fetches the catalog, conditionally when `etag` is known.
    ///
    /// # Errors
    ///
    /// The message names the transport or HTTP failure.
    async fn fetch(&self, etag: Option<&str>) -> Result<Fetched, String>;
}

/// The models.dev HTTP source, through the shared client settings.
pub struct ModelsDev {
    url: String,
    client: reqwest::Client,
}

impl ModelsDev {
    /// A source reading `url`, normally [`DEFAULT_URL`].
    pub fn new(url: impl Into<String>) -> Self {
        let client = rhema_provider::http::client_builder()
            .user_agent(USER_AGENT)
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self {
            url: url.into(),
            client,
        }
    }
}

impl Default for ModelsDev {
    fn default() -> Self {
        Self::new(DEFAULT_URL)
    }
}

#[async_trait]
impl Source for ModelsDev {
    async fn fetch(&self, etag: Option<&str>) -> Result<Fetched, String> {
        let mut request = self.client.get(&self.url);
        if let Some(etag) = etag {
            request = request.header(reqwest::header::IF_NONE_MATCH, etag);
        }
        let response = request
            .timeout(Duration::from_secs(30))
            .send()
            .await
            .map_err(|err| format!("models.dev: {err}"))?;
        if response.status() == reqwest::StatusCode::NOT_MODIFIED {
            return Ok(Fetched::NotModified);
        }
        if !response.status().is_success() {
            return Err(format!("models.dev answered {}", response.status()));
        }
        let etag = response
            .headers()
            .get(reqwest::header::ETAG)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let body = response
            .text()
            .await
            .map_err(|err| format!("models.dev body: {err}"))?;
        Ok(Fetched::Fresh { body, etag })
    }
}

/// The cache file: the body models.dev served, the validator for the
/// next request, and when this copy arrived. One file so a cache read
/// cannot pair one fetch's `ETag` with another's body.
#[derive(Deserialize)]
struct Cached {
    #[serde(default)]
    etag: Option<String>,
    fetched_at: u64,
    catalog: Catalog,
}

/// The write-side view: serializing borrows the catalog instead of
/// cloning the whole projection.
#[derive(Serialize)]
struct CachedRef<'a> {
    etag: Option<&'a str>,
    fetched_at: u64,
    catalog: &'a Catalog,
}

/// The in-process state behind one lock: a fetch holds it, so two
/// listings cannot download the same 6.9 MB twice.
#[derive(Default)]
struct State {
    catalog: Option<Arc<Catalog>>,
    etag: Option<String>,
    fetched_at: Option<SystemTime>,
    next_attempt: Option<SystemTime>,
}

/// The catalog cache: memory first, then the disk file, then the
/// network. A fetch failure keeps the last catalog, stale or not; no
/// catalog at all leaves the cards as the adapter listed them.
pub struct CatalogCache {
    state: Mutex<State>,
    source: Arc<dyn Source>,
    path: Option<PathBuf>,
    ttl: Duration,
    retry_after: Duration,
    timeout: Duration,
}

impl CatalogCache {
    /// A cache reading `source` and writing `path`. `None` keeps the
    /// catalog in memory only.
    pub fn new(source: Arc<dyn Source>, path: Option<PathBuf>) -> Self {
        Self {
            state: Mutex::new(State::default()),
            source,
            path,
            ttl: CACHE_TTL,
            retry_after: RETRY_AFTER,
            timeout: FETCH_TIMEOUT,
        }
    }

    /// The current catalog, fetching or reloading when needed.
    ///
    /// The order is memory, the disk file (a new process starts here),
    /// then the network. A stale catalog is returned when the network
    /// fails, so facts outlive the outage.
    pub async fn get(&self) -> Option<Arc<Catalog>> {
        let mut state = self.state.lock().await;
        let now = SystemTime::now();
        if state.catalog.is_none()
            && let Some((catalog, etag, fetched_at)) = self.read_disk().await
        {
            state.catalog = Some(Arc::new(catalog));
            state.etag = etag;
            state.fetched_at = Some(fetched_at);
        }
        if let Some(catalog) = &state.catalog
            && fresh(state.fetched_at, now, self.ttl)
        {
            return Some(Arc::clone(catalog));
        }
        if state.next_attempt.is_some_and(|at| now < at) {
            return state.catalog.clone();
        }
        match tokio::time::timeout(self.timeout, self.source.fetch(state.etag.as_deref())).await {
            Ok(Ok(Fetched::Fresh { body, etag })) => match Catalog::parse(&body) {
                Ok(catalog) => {
                    state.catalog = Some(Arc::new(catalog));
                    state.etag = etag.clone();
                    state.fetched_at = Some(now);
                    state.next_attempt = None;
                    if let Some(catalog) = &state.catalog {
                        self.write_disk(catalog, etag.as_deref(), now).await;
                    }
                }
                Err(_) => state.next_attempt = Some(now + self.retry_after),
            },
            Ok(Ok(Fetched::NotModified)) => {
                if state.catalog.is_some() {
                    state.fetched_at = Some(now);
                    state.next_attempt = None;
                } else {
                    // The server claims a body we do not have.
                    state.next_attempt = Some(now + self.retry_after);
                }
            }
            _ => state.next_attempt = Some(now + self.retry_after),
        }
        state.catalog.clone()
    }

    /// Reads the cache file. A missing, unreadable, or malformed file
    /// is simply no cache.
    async fn read_disk(&self) -> Option<(Catalog, Option<String>, SystemTime)> {
        let path = self.path.as_ref()?;
        let text = tokio::fs::read_to_string(path).await.ok()?;
        let cached: Cached = serde_json::from_str(&text).ok()?;
        let fetched_at = UNIX_EPOCH
            .checked_add(Duration::from_secs(cached.fetched_at))
            .unwrap_or(UNIX_EPOCH);
        Some((cached.catalog, cached.etag, fetched_at))
    }

    /// Writes the cache file through a temporary name, so a crash
    /// mid-write leaves the old copy instead of half a document.
    async fn write_disk(&self, catalog: &Catalog, etag: Option<&str>, at: SystemTime) {
        let Some(path) = &self.path else {
            return;
        };
        let Ok(text) = serde_json::to_string(&CachedRef {
            etag,
            fetched_at: unix(at),
            catalog,
        }) else {
            return;
        };
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await.ok();
        }
        let temporary = path.with_extension(format!("{}.tmp", std::process::id()));
        if tokio::fs::write(&temporary, text).await.is_ok() {
            tokio::fs::rename(&temporary, path).await.ok();
        }
    }
}

/// Whether a fetch at `at` is still inside `ttl`. A timestamp in the
/// future counts as fresh, a moved clock must not force a fetch loop.
fn fresh(at: Option<SystemTime>, now: SystemTime, ttl: Duration) -> bool {
    at.is_some_and(|at| now.duration_since(at).map_or(true, |age| age < ttl))
}

/// Unix seconds, the cache file's timestamp format.
fn unix(at: SystemTime) -> u64 {
    at.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs()
}

/// The process-wide cache the registry's listings share.
static SHARED: OnceLock<Arc<CatalogCache>> = OnceLock::new();

/// The shared cache, built on first use from the default source and
/// the user's cache directory.
pub(crate) fn shared() -> &'static Arc<CatalogCache> {
    SHARED.get_or_init(|| {
        Arc::new(CatalogCache::new(
            Arc::new(ModelsDev::default()),
            cache_path(),
        ))
    })
}

/// Installs the shared cache for an integration test, before the first
/// listing asks for it. Returns `false` when something already
/// installed one.
#[cfg(feature = "testing")]
pub fn install_for_tests(cache: Arc<CatalogCache>) -> bool {
    SHARED.set(cache).is_ok()
}

/// `$XDG_CACHE_HOME/rhema/models-dev.json`, or `~/.cache/...` when the
/// variable is unset. No directory is a cache miss, not an error.
fn cache_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")))?;
    Some(base.join("rhema").join("models-dev.json"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    const BODY: &str = r#"{
      "anthropic": { "models": {
        "claude-sonnet-4-6": {
          "name": "Claude Sonnet 4.6",
          "cost": { "input": 3, "output": 15 },
          "limit": { "context": 1000000 },
          "modalities": { "input": ["text", "image"] }
        }
      }},
      "opencode": { "models": {} }
    }"#;

    /// A source that answers from a fixed body and counts requests.
    #[derive(Default)]
    struct Scripted {
        calls: AtomicUsize,
        fail: AtomicBool,
        last_etag: std::sync::Mutex<Option<String>>,
    }

    impl Scripted {
        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }

        fn last_etag(&self) -> Option<String> {
            self.last_etag.lock().expect("the test lock").clone()
        }
    }

    #[async_trait]
    impl Source for Scripted {
        async fn fetch(&self, etag: Option<&str>) -> Result<Fetched, String> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            *self.last_etag.lock().expect("the test lock") = etag.map(str::to_owned);
            if self.fail.load(Ordering::SeqCst) {
                return Err("offline".to_owned());
            }
            if etag == Some("\"v1\"") {
                return Ok(Fetched::NotModified);
            }
            Ok(Fetched::Fresh {
                body: BODY.to_owned(),
                etag: Some("\"v1\"".to_owned()),
            })
        }
    }

    fn cache(source: Arc<Scripted>, path: Option<PathBuf>) -> CatalogCache {
        CatalogCache::new(source, path)
    }

    fn as_source(source: Arc<Scripted>) -> Arc<dyn Source> {
        source
    }

    fn temp_path(name: &str) -> PathBuf {
        let path =
            std::env::temp_dir().join(format!("rhema-catalog-{}-{name}.json", std::process::id()));
        let _ = std::fs::remove_file(&path);
        path
    }

    #[tokio::test]
    async fn the_first_get_fetches_and_the_next_reuses_memory() {
        let source = Arc::new(Scripted::default());
        let cache = cache(Arc::clone(&source), None);
        let first = cache.get().await.expect("the scripted body parses");
        assert_eq!(
            first
                .facts("anthropic", "claude-sonnet-4-6")
                .unwrap()
                .input_token_limit,
            Some(1_000_000)
        );
        assert!(cache.get().await.is_some());
        assert_eq!(source.calls(), 1);
    }

    #[tokio::test]
    async fn a_stale_entry_revalidates_with_the_stored_etag() {
        let source = Arc::new(Scripted::default());
        let cache = CatalogCache {
            state: Mutex::new(State::default()),
            source: as_source(Arc::clone(&source)),
            path: None,
            ttl: Duration::ZERO,
            retry_after: RETRY_AFTER,
            timeout: FETCH_TIMEOUT,
        };
        assert!(cache.get().await.is_some());
        assert!(cache.get().await.is_some());
        assert_eq!(source.calls(), 2);
        assert_eq!(source.last_etag().as_deref(), Some("\"v1\""));
    }

    #[tokio::test]
    async fn a_failed_fetch_keeps_the_last_catalog_and_waits_before_retrying() {
        let source = Arc::new(Scripted::default());
        let cache = CatalogCache {
            state: Mutex::new(State::default()),
            source: as_source(Arc::clone(&source)),
            path: None,
            ttl: Duration::ZERO,
            retry_after: Duration::from_secs(60),
            timeout: FETCH_TIMEOUT,
        };
        assert!(cache.get().await.is_some());
        source.fail.store(true, Ordering::SeqCst);
        assert!(cache.get().await.is_some(), "the stale catalog survives");
        assert!(cache.get().await.is_some());
        // The second failed call is inside the retry window: no third
        // request.
        assert_eq!(source.calls(), 2);
    }

    #[tokio::test]
    async fn a_new_process_reads_the_disk_before_the_network() {
        let path = temp_path("disk");
        let source = Arc::new(Scripted::default());
        let writer = cache(Arc::clone(&source), Some(path.clone()));
        assert!(writer.get().await.is_some());
        assert_eq!(source.calls(), 1);

        let offline = Arc::new(Scripted::default());
        offline.fail.store(true, Ordering::SeqCst);
        let reopened = cache(Arc::clone(&offline), Some(path.clone()));
        let catalog = reopened.get().await.expect("the disk copy loads");
        assert_eq!(
            catalog
                .facts("anthropic", "claude-sonnet-4-6")
                .and_then(|facts| facts.input_token_limit),
            Some(1_000_000)
        );
        // Fresh on disk, so the network is never asked.
        assert_eq!(offline.calls(), 0);
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn no_cache_and_a_failed_fetch_reads_as_no_catalog() {
        let source = Arc::new(Scripted::default());
        source.fail.store(true, Ordering::SeqCst);
        let cache = cache(Arc::clone(&source), None);
        assert!(cache.get().await.is_none());
        assert!(cache.get().await.is_none());
        assert_eq!(source.calls(), 1);
    }

    #[test]
    fn fresh_treats_an_unset_or_future_time_as_fresh() {
        let now = SystemTime::now();
        assert!(!fresh(None, now, CACHE_TTL));
        assert!(fresh(Some(now + Duration::from_secs(60)), now, CACHE_TTL));
        assert!(fresh(Some(now), now, CACHE_TTL));
        assert!(!fresh(Some(now - CACHE_TTL), now, CACHE_TTL));
    }
}
