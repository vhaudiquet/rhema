//! The sources: deterministic detection, one [`ForgeSource`] per forge
//! kind present in the watches.
//!
//! A source owns one forge's watch set: its `detect` pass loops the
//! kind's repos through a [`Watcher`] — poll, ingest, advance the
//! per-repo cursor — and never touches the model (spec §1). Cadence
//! belongs to the daemon's loop; the source is one pass, nothing more.

use std::sync::Arc;

use rhema_autonomous_agent::{Source, SourceError, async_trait};
use rhema_forge_agent::{
    Forge, GitHubForge, GitLabForge, GiteaForge, RepoRef, WatchError, Watcher, register_forge,
};
// `forge_key_fn` is public in the watcher module but not re-exported at
// the crate root, unlike its siblings — reached through the module path.
use rhema_forge_agent::forge_key_fn;
use rhema_rsd::{Dispatcher, KeyFn};

use crate::config::Watch;

/// One watched repo and the client that polls it: the unit a source's
/// detect pass loops over.
#[derive(Clone)]
pub struct Target {
    /// The repository coordinates.
    pub repo: RepoRef,
    /// The forge client this repo polls through.
    pub forge: Arc<dyn Forge>,
}

/// The forge client one watch polls through: the kind's production base
/// unless the watch names a `host`. Assumes
/// [`ClawConfig::validate`](crate::ClawConfig::validate) ran — a hostless
/// gitea watch is a config error caught there, and a tokenless one never
/// reaches a client.
pub fn forge_client(watch: &Watch) -> Arc<dyn Forge> {
    let token = watch.resolve_token().unwrap_or_default();
    match watch.forge {
        crate::config::ForgeKind::GitHub => {
            let mut forge = GitHubForge::new(token);
            if let Some(host) = &watch.host {
                forge = forge.with_base_url(host.clone());
            }
            Arc::new(forge)
        }
        crate::config::ForgeKind::Gitea => Arc::new(GiteaForge::new(
            watch.host.clone().unwrap_or_default(),
            token,
        )),
        crate::config::ForgeKind::GitLab => {
            let mut forge = GitLabForge::new(token);
            if let Some(host) = &watch.host {
                forge = forge.with_base_url(host.clone());
            }
            Arc::new(forge)
        }
    }
}

/// One forge kind's detectable input stream: every watch of that kind in
/// the config, polled through per-repo cursors into the dispatcher.
pub struct ForgeSource {
    name: String,
    watcher: Watcher,
    targets: Vec<Target>,
}

impl ForgeSource {
    /// Registers `name`'s key function on the dispatcher and returns the
    /// source polling `targets` under it. One source per kind, registered
    /// once, before the first detect.
    pub fn new(
        dispatcher: &Arc<Dispatcher>,
        name: impl Into<String>,
        targets: Vec<Target>,
    ) -> Self {
        let name = name.into();
        register_forge(dispatcher, &name);
        Self {
            watcher: Watcher::new(dispatcher.clone(), name.clone()),
            name,
            targets,
        }
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
        let mut fed = 0;
        for target in &self.targets {
            fed += self
                .watcher
                .poll(target.forge.as_ref(), &target.repo)
                .await
                .map_err(|err| match err {
                    // The forge call is the source's own backend; a
                    // dispatcher rejection keeps its own variant.
                    WatchError::Forge(err) => {
                        SourceError::Backend(format!("{}: {err}", target.repo.path()))
                    }
                    WatchError::Dispatch(err) => err.into(),
                })?;
        }
        Ok(fed)
    }
}
