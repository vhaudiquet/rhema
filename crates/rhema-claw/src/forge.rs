//! The sources: deterministic detection, one [`ForgeSource`] per forge
//! kind.
//!
//! A source owns one forge's watch set: its `detect` pass reads the
//! watch tables fresh — [`ClawStore::list`] and [`ClawStore::
//! list_account_watches`] — and loops the kind's repos through a
//! [`Watcher`] (poll, ingest, advance the per-repo cursor), then the
//! kind's account watches through the identity store's credentials:
//! poll the account's notifications, [`crate::triage::classify`] each
//! one, and either ingest it as a role-tagged thread event, surface it
//! to the account's human, or drop it. Neither pass touches the model
//! (spec §1). Reading the tables every pass is what makes a watch added
//! mid-flight (chat tool, HTTP route) take effect without a restart.
//! Repo-watch clients are constructed on first use and cached per
//! `(forge, host, token)`; account clients are built fresh every pass,
//! so a rotated identity secret is live on the next tick. Cadence
//! belongs to the daemon's loop; the source is one pass, nothing more.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};

use rhema_autonomous_agent::{Source, SourceError, async_trait};
use rhema_forge_integration::forge_key_fn;
use rhema_forge_integration::{
    Forge, GitHubForge, GitLabForge, GiteaForge, Notification, Watcher, register_forge,
};
use rhema_identity::{IdentityStore, SecretKind};
use rhema_rsd::{Clock, Dispatcher, Event, KeyFn, SystemClock};

use crate::config::{ForgeKind, resolve_token};
use crate::triage::{self, Triage};
use crate::watch_store::{AccountWatchRecord, ClawStore, WatchRecord};

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

/// One forge kind's detectable input stream: every repo watch and every
/// account watch of that kind in the tables, polled into the dispatcher.
pub struct ForgeSource {
    name: String,
    watcher: Watcher,
    dispatcher: Arc<Dispatcher>,
    store: Arc<ClawStore>,
    /// The account store the account watches authenticate through. A
    /// secret stays inside this crate's code paths: it reaches the forge
    /// client's constructor, never a log line or a tool answer.
    accounts: Arc<dyn IdentityStore>,
    /// The device-notification queue a [`Triage::Surface`] lands in.
    notifications: Arc<crate::notifications::NotificationStore>,
    clients: Mutex<HashMap<ClientKey, Arc<dyn Forge>>>,
    /// One notifications cursor per account id.
    account_cursors: Mutex<HashMap<String, rhema_forge_integration::Timestamp>>,
}

impl ForgeSource {
    /// Registers `name`'s key function on the dispatcher and returns the
    /// source polling the watch table's `name` rows — repo watches and
    /// account watches — under it. One source per kind, registered once,
    /// before the first detect.
    pub fn new(
        dispatcher: &Arc<Dispatcher>,
        name: impl Into<String>,
        store: Arc<ClawStore>,
        accounts: Arc<dyn IdentityStore>,
        notifications: Arc<crate::notifications::NotificationStore>,
    ) -> Self {
        let name = name.into();
        register_forge(dispatcher, &name);
        Self {
            watcher: Watcher::new(dispatcher.clone(), name.clone()),
            dispatcher: dispatcher.clone(),
            name,
            store,
            accounts,
            notifications,
            clients: Mutex::new(HashMap::new()),
            account_cursors: Mutex::new(HashMap::new()),
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
        // The account watches: the same table-rules-every-pass doctrine,
        // with the identity store supplying each poll's credential.
        let accounts = self
            .store
            .list_account_watches()
            .map_err(|err| SourceError::Backend(format!("watch store: {err}")))?;
        for watch in accounts {
            if watch.forge != self.name {
                continue;
            }
            match self.poll_account(&watch).await {
                Ok(count) => fed += count,
                // One dead account (a revoked token, a forge outage)
                // must not starve the kind's other watches either.
                Err(err) => eprintln!(
                    "[claw] warning: account watch {} {}: poll failed: {err}",
                    watch.forge, watch.account_id
                ),
            }
        }
        Ok(fed)
    }
}

impl ForgeSource {
    /// One account-watch poll: fetch the account's notifications as the
    /// account, triage each, and land the verdicts — agent dispatches
    /// ingest as role-tagged thread events, surfaces hit the
    /// device-notification queue, ignores drop. Returns how many events
    /// were fed to the dispatcher.
    async fn poll_account(&self, watch: &AccountWatchRecord) -> Result<usize, SourceError> {
        let Some(kind) = ForgeKind::parse(&watch.forge) else {
            // Unreachable through the daemon's assembly, which only
            // creates sources for known kinds; an unknown kind in the
            // table would be skipped in `detect` before reaching here.
            unreachable!("detect filters unknown forge kinds before account polls")
        };
        let secret = self
            .accounts
            .secret(&watch.account_id)
            .map_err(|err| SourceError::Backend(format!("account store: {err}")))?;
        let Some(secret) = secret else {
            // The account is gone; its watch's cascade should have gone
            // with it. Skip quietly — `identity_forget` removes both,
            // and a hand-edited pair self-heals on the next forget.
            return Ok(0);
        };
        if secret.kind != SecretKind::Token {
            // A stored password is not a poll credential: nothing to
            // authenticate the notifications call with. Say so and move
            // on — the watch stays, a token rotation (or a future login
            // action) revives it.
            eprintln!(
                "[claw] warning: account watch {} {}: the account holds a password, not a token; skipping the poll",
                watch.forge, watch.account_id
            );
            return Ok(0);
        }
        // Fresh client every pass: a rotated secret is live on the next
        // tick, and one client build per account per pass is nothing
        // next to the poll itself.
        let client = record_client(kind, watch.host.as_deref(), &secret.value);
        let since = self
            .account_cursors
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&watch.account_id)
            .cloned();
        let page = client
            .notifications(since.as_ref())
            .await
            .map_err(|err| SourceError::Backend(format!("notifications: {err}")))?;
        let now = SystemClock.now_millis();
        let mut fed = 0;
        for notification in &page.notifications {
            match triage::classify(notification) {
                Triage::Ignore => {}
                Triage::Surface => {
                    if let Err(err) = self.notifications.push(
                        crate::notifications::DEFAULT_DEVICE,
                        &format!(
                            "{}: {}#{}",
                            watch.forge,
                            notification.repo.path(),
                            notification.number
                        ),
                        &surface_body(notification),
                    ) {
                        eprintln!(
                            "[claw] warning: surface of {} failed: {err}",
                            notification.id
                        );
                    }
                }
                Triage::Agent(role) => {
                    // The key function reads `repo` (as the
                    // `{owner, name}` row RepoRef serializes to) and
                    // `number` off the payload's top level, so the
                    // thread key is the same `forge:owner/repo#n` shape
                    // the repo watches use — one session per thread,
                    // whatever fed it.
                    let payload = serde_json::json!({
                        "repo": notification.repo,
                        "number": notification.number,
                        "role": role.as_str(),
                        "notification": notification,
                    });
                    self.dispatcher
                        .ingest(Event {
                            id: notification.id.clone(),
                            source: self.name.clone(),
                            payload,
                            received_at: now,
                        })
                        .map_err(SourceError::Dispatch)?;
                    fed += 1;
                }
            }
        }
        if let Some(cursor) = page.cursor {
            self.account_cursors
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .insert(watch.account_id.clone(), cursor);
        }
        Ok(fed)
    }
}

/// The device-notification text a [`Triage::Surface`] lands: the reason
/// and the thread title, so the human can judge without opening the
/// forge.
fn surface_body(notification: &Notification) -> String {
    let title = notification.title.as_deref().unwrap_or("(untitled)");
    format!(
        "{} on {}#{}: {}",
        notification.reason.as_str(),
        notification.repo.path(),
        notification.number,
        title
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The parts a test source polls through, all in memory.
    fn test_parts(
        store: Arc<ClawStore>,
    ) -> (
        Arc<Dispatcher>,
        Arc<dyn IdentityStore>,
        Arc<crate::notifications::NotificationStore>,
    ) {
        let _ = store;
        let dispatcher = Arc::new(Dispatcher::new(
            rhema_rsd::DispatcherConfig::default(),
            Arc::new(rhema_rsd::SystemClock),
            Arc::new(rhema_rsd::VolatileStore::new()),
        ));
        let accounts: Arc<dyn IdentityStore> =
            Arc::new(rhema_identity::VolatileIdentityStore::new());
        let notifications = Arc::new(
            crate::notifications::NotificationStore::in_memory().expect("notification store opens"),
        );
        (dispatcher, accounts, notifications)
    }

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
        let (dispatcher, accounts, notifications) = test_parts(store.clone());
        let source = ForgeSource::new(&dispatcher, "github", store, accounts, notifications);

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

    /// One triaged notification ingest: the payload carries `repo` and
    /// `number` at the top level, so the registered key function folds
    /// it onto the same thread key a repo-watch event takes.
    #[tokio::test]
    async fn a_triaged_notification_lands_on_the_thread_key() {
        use rhema_forge_integration::{NotificationReason, RepoRef, Timestamp};

        let store = Arc::new(ClawStore::in_memory().expect("store opens"));
        let (_dispatcher, accounts, _notifications) = test_parts(store.clone());
        let account = rhema_identity::AccountDraft::new(
            "acme bot",
            "github",
            rhema_identity::Secret::new(rhema_identity::SecretKind::Token, "tok"),
        );
        let account = accounts.put(account).expect("account lands");
        store
            .add_account_watch("github", None, &account.id)
            .expect("watch lands");

        // The key function must fold the triaged payload onto the thread
        // key: build a payload the way `poll_account` does and run it
        // through the registered `forge_key_fn`.
        let notification = Notification {
            id: "github:notification:7@2026-10-08T00:00:00Z".to_owned(),
            reason: NotificationReason::ReviewRequested,
            repo: RepoRef::new("acme", "widgets"),
            number: 142,
            is_pr: true,
            actor: None,
            title: Some("Add the flange".to_owned()),
            url: None,
            timestamp: Timestamp::parse("2026-10-08T00:00:00Z").expect("parses"),
            raw: serde_json::json!({}),
        };
        let payload = serde_json::json!({
            "repo": notification.repo,
            "number": notification.number,
            "role": "reviewer",
            "notification": notification,
        });
        let key_fn = forge_key_fn("github");
        let key = key_fn(&Event {
            id: notification.id.clone(),
            source: "github".to_owned(),
            payload,
            received_at: 0,
        });
        assert_eq!(key.as_str(), "github:acme/widgets#142");
    }
}
