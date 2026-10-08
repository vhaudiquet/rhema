//! The daemon: sources, driver, and cadence assembled into one loop.
//!
//! Every timer here drives deterministic detection and admission only —
//! the model is reached exclusively inside handler runs (spec §7's
//! invariant: a resident process is not a process that burns tokens on a
//! schedule). The `--once` shape is the same assembly with a zero
//! debounce window — a cron pass acts on what it sees now — and one
//! detect-plus-driver pass instead of a loop.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::Duration;

use rhema_autonomous_agent::{Driver, Source, SourceError};
use rhema_core::ModelProvider;
use rhema_rsd::{
    DispatchError, Dispatcher, DispatcherConfig, Outcome, SqliteStore, StoreError, SystemClock,
};

use crate::config::{ClawConfig, ConfigError};
use crate::forge::{ForgeSource, Target, forge_client};
use crate::handler::ClawHandler;

/// Why the daemon could not run.
#[derive(Debug, thiserror::Error)]
pub enum ClawError {
    /// The config file is missing, malformed, or fails validation.
    #[error(transparent)]
    Config(#[from] ConfigError),
    /// A directory or path the daemon needs could not be created.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// The SQLite store could not be opened.
    #[error(transparent)]
    Store(#[from] StoreError),
    /// A source's detect pass failed.
    #[error(transparent)]
    Source(#[from] SourceError),
    /// The dispatcher rejected a step.
    #[error(transparent)]
    Dispatch(#[from] DispatchError),
}

/// One daemon pass: what detection fed, and what the runs reported.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TickReport {
    /// Events fed by detection across all sources — replayed dedup ids
    /// included; the dispatcher dedups.
    pub detected: usize,
    /// One outcome per key that became a run; a key that was parked or
    /// raced is simply absent.
    pub outcomes: Vec<Outcome>,
}

/// The resident daemon: one source per forge kind, the driver over the
/// handler, and the poll cadence.
pub struct Daemon {
    sources: Vec<ForgeSource>,
    driver: Driver,
    poll_interval: Duration,
}

impl Daemon {
    /// Assembles the daemon over `config`: the SQLite-backed dispatcher,
    /// one source per forge kind present in the watches, and the handler
    /// running `provider`. `dispatch` is the dispatcher's timing — the
    /// resident loop uses the defaults, `--once` zeroes the debounce.
    pub fn build(
        config: &ClawConfig,
        provider: Arc<dyn ModelProvider>,
        dispatch: DispatcherConfig,
    ) -> Result<Self, ClawError> {
        config.validate()?;
        let db = config.db_path()?;
        if let Some(parent) = db.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let workspace_root = config.workspace_root()?;
        std::fs::create_dir_all(&workspace_root)?;

        let store = SqliteStore::open(&db)?;
        let dispatcher = Arc::new(Dispatcher::new(
            dispatch,
            Arc::new(SystemClock),
            Arc::new(store),
        ));

        // The watch table the dispatch keys resolve against, and the
        // per-kind target lists the sources poll: same clients, built
        // once. BTreeMap keeps the source order deterministic.
        let mut by_kind: BTreeMap<&'static str, Vec<Target>> = BTreeMap::new();
        let mut forges: HashMap<String, Arc<dyn rhema_forge_agent::Forge>> = HashMap::new();
        for (index, watch) in config.watches.iter().enumerate() {
            // `validate` checked every watch already; the map_err keeps a
            // bad shape named rather than assumed impossible.
            let repo = watch.repo_ref().map_err(|reason| ConfigError::Watch {
                index,
                forge: watch.forge.name().to_owned(),
                repo: watch.repo.clone(),
                reason,
            })?;
            let client = forge_client(watch);
            forges.insert(
                crate::handler::watch_entry(watch.forge.name(), &repo),
                client.clone(),
            );
            by_kind.entry(watch.forge.name()).or_default().push(Target {
                repo,
                forge: client,
            });
        }

        let sources = by_kind
            .into_iter()
            .map(|(kind, targets)| ForgeSource::new(&dispatcher, kind, targets))
            .collect();

        let handler = ClawHandler::new(provider, config.resolved_model(), workspace_root, forges);
        Ok(Self {
            sources,
            driver: Driver::new(dispatcher, Arc::new(handler)).with_holder("rhema-claw"),
            // A zero poll interval would spin detect passes against the
            // forge API; one second is the floor.
            poll_interval: Duration::from_secs(config.poll_interval_secs.max(1)),
        })
    }

    /// Startup recovery (RSD spec §7): sweeps leases a dead process left
    /// behind. Idempotent; called before the first pass.
    pub async fn bootstrap(&self) -> Result<(), DispatchError> {
        self.driver.bootstrap().await
    }

    /// One pass: every source detects, then the driver runs every key the
    /// debounce released.
    pub async fn tick(&self) -> Result<TickReport, ClawError> {
        let mut detected = 0;
        for source in &self.sources {
            detected += source.detect().await?;
        }
        let outcomes = self.driver.run_once().await?;
        if !outcomes.is_empty() {
            let handled = outcomes
                .iter()
                .filter(|outcome| matches!(outcome, Outcome::Handled))
                .count();
            let failed = outcomes
                .iter()
                .filter(|outcome| matches!(outcome, Outcome::Failed(_)))
                .count();
            eprintln!(
                "[claw] pass: {detected} event(s) detected, {handled} handled, {failed} failed"
            );
        }
        Ok(TickReport { detected, outcomes })
    }

    /// The resident loop: detect, run, sleep, until ctrl-c. A failed pass
    /// is logged, not fatal — a forge outage or a dead backend must not
    /// end a daemon whose next pass may simply succeed.
    pub async fn run(&self) -> Result<(), ClawError> {
        self.bootstrap().await?;
        loop {
            if let Err(err) = self.tick().await {
                eprintln!("[claw] warning: pass failed: {err}");
            }
            tokio::select! {
                () = tokio::time::sleep(self.poll_interval) => {}
                _ = tokio::signal::ctrl_c() => return Ok(()),
            }
        }
    }
}

/// The `--once` shape: assemble over a zero debounce window, recover, run
/// one detect-plus-driver pass, and return. The cron-friendly form for
/// ephemeral machines, and the shape the tests drive.
pub async fn run_once(
    config: &ClawConfig,
    provider: Arc<dyn ModelProvider>,
) -> Result<TickReport, ClawError> {
    let dispatch = DispatcherConfig {
        debounce_window_millis: 0,
        ..DispatcherConfig::default()
    };
    let daemon = Daemon::build(config, provider, dispatch)?;
    daemon.bootstrap().await?;
    daemon.tick().await
}
