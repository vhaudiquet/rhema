//! The sources: deterministic detection, one [`ForgeSource`] per forge
//! kind.
//!
//! A source owns one forge's watch set: its `detect` pass reads the
//! watch table fresh ([`ClawStore::list`]) and loops the kind's repos
//! through a [`Watcher`] — poll, ingest, advance the per-repo cursor —
//! and never touches the model (spec §1). Reading the table every pass
//! is what makes a watch added mid-flight (chat tool, HTTP route) take
//! effect without a restart. Forge clients are constructed on first use
//! and cached per `(forge, host, token)`. Cadence belongs to the
//! daemon's loop; the source is one pass, nothing more.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};

use rhema_autonomous_agent::{Source, SourceError, async_trait};
use rhema_forge_integration::forge_key_fn;
use rhema_forge_integration::{
    Forge, GitHubForge, GitLabForge, GiteaForge, Watcher, register_forge,
};
use rhema_rsd::{Dispatcher, KeyFn};

use crate::config::{ForgeKind, resolve_token};
use crate::watch_store::{ClawStore, WatchRecord};

/// The cache key of a forge client: everything that changes the client
/// is the forge kind, its host, and the token it authenticates with.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ClientKey {
    forge: String,
    host: Option<String>,
    token: String,
}

/// The forge client one watch RECORD polls through: the kind's
/// production base unless the record names a `host`. A blank stored
/// token falls back to the forge's environment variable — the same
/// resolution a config watch gets — and a blank everywhere constructs a
/// client that polls anonymous; the poll fails loudly at the forge, not
/// silently here.
pub fn record_client(kind: ForgeKind, host: Option<&str>, token: &str) -> Arc<dyn Forge> {
    let resolved = resolve_token(|name| std::env::var(name), kind, Some(token)).unwrap_or_default();
    match kind {
        ForgeKind::GitHub => {
            let mut forge = GitHubForge::new(resolved);
            if let Some(host) = host {
                forge = forge.with_base_url(host.to_owned());
            }
            Arc::new(forge)
        }
        ForgeKind::Gitea => Arc::new(GiteaForge::new(
            host.unwrap_or_default().to_owned(),
            resolved,
        )),
        ForgeKind::GitLab => {
            let mut forge = GitLabForge::new(resolved);
            if let Some(host) = host {
                forge = forge.with_base_url(host.to_owned());
            }
            Arc::new(forge)
        }
    }
}

/// One forge kind's detectable input stream: every watch of that kind in
/// the table, polled through per-repo cursors into the dispatcher.
pub struct ForgeSource {
    name: String,
    watcher: Watcher,
    store: Arc<ClawStore>,
    clients: Mutex<HashMap<ClientKey, Arc<dyn Forge>>>,
}

impl ForgeSource {
    /// Registers `name`'s key function on the dispatcher and returns the
    /// source polling the watch table's `name` rows under it. One source
    /// per kind, registered once, before the first detect.
    pub fn new(
        dispatcher: &Arc<Dispatcher>,
        name: impl Into<String>,
        store: Arc<ClawStore>,
    ) -> Self {
        let name = name.into();
        register_forge(dispatcher, &name);
        Self {
            watcher: Watcher::new(dispatcher.clone(), name.clone()),
            name,
            store,
            clients: Mutex::new(HashMap::new()),
        }
    }

    /// The client for `record`: from the cache when a watch with the
    /// same `(forge, host, token)` polled before, constructed otherwise.
    fn client_for(&self, record: &WatchRecord) -> Arc<dyn Forge> {
        let key = ClientKey {
            forge: record.forge.clone(),
            host: record.host.clone(),
            token: record.token.clone(),
        };
        let mut clients = self.clients.lock().unwrap_or_else(PoisonError::into_inner);
        clients
            .entry(key)
            .or_insert_with(|| {
                let Some(kind) = ForgeKind::parse(&record.forge) else {
                    // Unreachable through the daemon's assembly, which
                    // only creates sources for known kinds; an unknown
                    // kind in the table would be skipped in `detect`
                    // before reaching here.
                    unreachable!("detect filters unknown forge kinds before client lookup")
                };
                record_client(kind, record.host.as_deref(), &record.token)
            })
            .clone()
    }
}

#[async_trait]
impl Source for ForgeSource {
    fn name(&self) -> &str {
        &self.name
    }

    fn key_fn(&self) -> Arc<KeyFn> {
        forge_key_fn(&self.name)
    }

    async fn detect(&self) -> Result<usize, SourceError> {
        let records = self
            .store
            .list()
            .map_err(|err| SourceError::Backend(format!("watch store: {err}")))?;
        let mut fed = 0;
        for record in records {
            if record.forge != self.name {
                continue;
            }
            let repo = match record.repo_ref() {
                Ok(repo) => repo,
                Err(reason) => {
                    // A malformed row must not starve the kind's other
                    // watches; say so and move on. New rows cannot be
                    // malformed (every entry validates), so this guards
                    // only hand-edited tables.
                    eprintln!(
                        "[claw] warning: skipping watch {} {}: {reason}",
                        record.forge, record.repo
                    );
                    continue;
                }
            };
            let client = self.client_for(&record);
            match self.watcher.poll(client.as_ref(), &repo).await {
                Ok(count) => fed += count,
                // One dead watch (a lapsed token, a forge outage) must
                // not starve the kind's other watches either.
                Err(err) => eprintln!(
                    "[claw] warning: watch {} {}: poll failed: {err}",
                    record.forge, record.repo
                ),
            }
        }
        Ok(fed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The client cache pays off when two watches share their
    /// coordinates: one client, not two.
    #[tokio::test]
    async fn same_coordinates_share_one_cached_client() {
        let store = Arc::new(ClawStore::in_memory().expect("store opens"));
        store
            .add("github", "acme/widgets", None, "tok")
            .expect("add");
        store
            .add("github", "acme/gadgets", None, "tok")
            .expect("add");
        store
            .add("github", "acme/private", None, "other")
            .expect("add");
        let dispatcher = Arc::new(Dispatcher::new(
            rhema_rsd::DispatcherConfig::default(),
            Arc::new(rhema_rsd::SystemClock),
            Arc::new(rhema_rsd::VolatileStore::new()),
        ));
        let source = ForgeSource::new(&dispatcher, "github", store);

        let records = source.store.list().expect("list reads");
        let a = source.client_for(&records[0]);
        let b = source.client_for(&records[1]);
        let c = source.client_for(&records[2]);
        assert!(Arc::ptr_eq(&a, &b), "same (forge, host, token): one client");
        assert!(
            !Arc::ptr_eq(&a, &c),
            "a different token is a different client"
        );
        assert_eq!(source.clients.lock().unwrap().len(), 2);
    }
}
