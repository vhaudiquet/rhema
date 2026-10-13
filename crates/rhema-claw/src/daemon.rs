//! The daemon: sources, drivers, the HTTP surface, and the cadence
//! assembled into one resident process.
//!
//! Two dispatchers share one SQLite store: the forge dispatcher (watches
//! polled by the sources, answered by [`ClawHandler`]) and the
//! interactive dispatcher — the resident dispatcher — whose keys carry
//! the `interactive:`/`task:`/`phone:` prefixes and whose inputs are the
//! chat routes' messages, the `/v1/ingest` door, and the task
//! scheduler's due firings, all answered by the interactive handler.
//! Sharing the store is what makes the whole thing one resident agent
//! with one inventory — and it is safe because the store's whole-record
//! revision compare-and-swap serializes across every instance holding
//! the file (rhema-rsd's cross-connection CAS tests pin that).
//!
//! Every timer here drives deterministic detection and admission only —
//! the model is reached exclusively inside handler runs (spec §7's
//! invariant: a resident process is not a process that burns tokens on a
//! schedule). The `--once` shape is the same assembly with a zero
//! debounce window — a cron pass acts on what it sees now — and one
//! detect-plus-driver pass instead of a loop, with no HTTP surface: cron
//! machines have no one to chat with.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use rhema_autonomous_agent::{Driver, Source, SourceError};
use rhema_rsd::{
    DispatchError, Dispatcher, DispatcherConfig, Outcome, SqliteStore, StoreError, SystemClock,
};

use crate::config::{ClawConfig, ConfigError, ForgeKind};
use crate::forge::ForgeSource;
use crate::handler::ClawHandler;
use crate::http::ClawState;
use crate::notifications::NotificationStore;
use crate::task_source::TaskScheduler;
use crate::task_store::TaskStore;
use rhema_memory::FileMemoryStore;

use crate::backends::BackendRegistry;
use crate::hub::EventHub;
use crate::interactive::InteractiveHandler;
use crate::watch_store::ClawStore;

/// Why the daemon could not run.
#[derive(Debug, thiserror::Error)]
pub enum ClawError {
    /// The config file is missing, malformed, or fails validation.
    #[error(transparent)]
    Config(#[from] ConfigError),
    /// A directory or path the daemon needs could not be created, or the
    /// HTTP listener could not bind.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// The SQLite store could not be opened.
    #[error(transparent)]
    Store(#[from] StoreError),
    /// The watch table could not be opened or read.
    #[error(transparent)]
    Watches(#[from] crate::watch_store::WatchStoreError),
    /// The runtime settings table could not be opened or read.
    #[error(transparent)]
    Settings(#[from] crate::settings::SettingsError),
    /// The notification table could not be opened or read.
    #[error(transparent)]
    Notifications(#[from] crate::notifications::NotificationStoreError),
    /// The task table could not be opened or read.
    #[error(transparent)]
    Tasks(#[from] crate::task_store::TaskStoreError),
    /// The turn-history table could not be opened or read.
    #[error(transparent)]
    TurnHistory(#[from] crate::turn_store::TurnStoreError),
    /// The backend registry could not be built: no provider constructed,
    /// or the configured seed named an unknown kind.
    #[error("backend registry failed: {0}")]
    Backend(String),
    /// The bootstrap store could not be opened.
    #[error(transparent)]
    Bootstrap(#[from] crate::bootstrap::BootstrapError),
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
    /// One outcome per forge key that became a run; a key that was
    /// parked or raced is simply absent.
    pub outcomes: Vec<Outcome>,
}

/// The interactive dispatcher's timing: messages dispatch immediately —
/// an open surface has nothing to coalesce (spec §6.2) — leases run an
/// hour and renew at the handler's poll boundaries because turns can run
/// long, and the caps are generous for the same reason.
fn interactive_dispatch() -> DispatcherConfig {
    DispatcherConfig {
        max_concurrent_leases: 64,
        max_concurrent_per_source: BTreeMap::new(),
        debounce_window_millis: 0,
        lease_ttl_millis: 3_600_000,
        admission_ttl_millis: 60_000,
    }
}

/// The resident daemon: one source per forge kind, the task scheduler,
/// the two drivers, and the HTTP surface.
pub struct Daemon {
    sources: Vec<ForgeSource>,
    scheduler: TaskScheduler,
    forge_driver: Arc<Driver>,
    interactive_driver: Arc<Driver>,
    poll_interval: Duration,
    listen: String,
    http: ClawState,
    /// The registry the drivers' handlers read per dispatch; kept for
    /// introspection (the active pair after a bootstrap).
    registry: Arc<BackendRegistry>,
}

impl Daemon {
    /// Assembles the daemon over `config`: the shared SQLite store, one
    /// source per forge kind reading the watch table, the interactive
    /// dispatcher, and both handlers running `provider`. `dispatch` is
    /// the forge dispatcher's timing — the resident loop uses the
    /// defaults, `--once` zeroes the debounce.
    pub fn build(
        config: &ClawConfig,
        backends: Arc<BackendRegistry>,
        dispatch: DispatcherConfig,
    ) -> Result<Self, ClawError> {
        config.validate()?;
        let db = config.db_path()?;
        if let Some(parent) = db.parent() {
            // The store holds provider keys and forge tokens: `0700` on
            // the directory, `0600` on the file (below), both
            // re-asserted on every boot.
            rhema_config::privacy::private_dir(parent)?;
        }
        let workspace_root = config.workspace_root()?;
        let interactive_workspace = workspace_root.join("interactive");
        std::fs::create_dir_all(&interactive_workspace)?;

        // One store file, several connections: the dispatcher's records,
        // the watch table, the notification table, the task table, and
        // the turn-history table live side by side.
        let store = Arc::new(SqliteStore::open(&db)?);
        // SQLite created the file under the umask; whatever it left
        // wide, tighten. Best-effort: a filesystem that refuses mode
        // changes must not stop the boot.
        rhema_config::privacy::tighten_file(&db);
        let watches = Arc::new(ClawStore::open(&db)?);
        let notifications = Arc::new(NotificationStore::open(&db)?);
        let tasks = Arc::new(TaskStore::open(&db)?);
        let turns = Arc::new(crate::turn_store::TurnStore::open(&db)?);

        // The YAML watch list is the one-time seed; the table rules
        // after that.
        let seeded = watches.seed_from_config(&config.watches)?;
        if seeded > 0 {
            eprintln!("[claw] seeded {seeded} watch(es) from the config");
        }

        let forge_dispatcher = Arc::new(Dispatcher::new(
            dispatch,
            Arc::new(SystemClock),
            store.clone(),
        ));
        let interactive_dispatcher = Arc::new(Dispatcher::new(
            interactive_dispatch(),
            Arc::new(SystemClock),
            store.clone(),
        ));

        // The account store the forge sources poll through: an account
        // watch authenticates as its identity, and the secret reaches
        // the forge client's constructor and nowhere else.
        let accounts: Arc<dyn rhema_identity::IdentityStore> = Arc::new(
            rhema_identity::SqliteIdentityStore::open(&db)
                .map_err(|err| ClawError::Backend(err.to_string()))?,
        );

        // One source per known forge kind, whatever the table currently
        // holds: a watch added later (chat tool, HTTP route) of a kind
        // nobody watched at startup still has its key function
        // registered. A source over zero watches detects nothing.
        let sources = [ForgeKind::GitHub, ForgeKind::Gitea, ForgeKind::GitLab]
            .into_iter()
            .map(|kind| {
                ForgeSource::new(
                    &forge_dispatcher,
                    kind.name(),
                    watches.clone(),
                    accounts.clone(),
                    notifications.clone(),
                )
            })
            .collect();

        // The forge handler resolves its watches through the table at
        // dispatch time, so a watch added mid-run is answerable without a
        // restart.
        let forge_handler = ClawHandler::new(backends.clone(), workspace_root, watches.clone());
        let forge_driver = Arc::new(
            Driver::new(forge_dispatcher, Arc::new(forge_handler)).with_holder("rhema-claw"),
        );

        // The hub replays what past turns streamed: every event appends
        // to the turn-history table as it streams, so a restart's replay
        // reads the table back — a restart continues the transcript,
        // whole, with no boot-time seed pass.
        let hub = Arc::new(EventHub::with_record_store(turns.clone()));
        let memory = Arc::new(FileMemoryStore::new(config.memory_path()?)?);
        let bootstrap = Arc::new(crate::bootstrap::Bootstrap::new(config.bootstrap_dir()?)?);
        let settings = Arc::new(crate::settings::SettingsStore::open(&db)?);
        // The identity store shares the settings table and keeps its
        // images beside the database.
        let identity = Arc::new(crate::identity::IdentityStore::new(
            settings.clone(),
            db.parent().map(|dir| dir.join("identity")).ok_or_else(|| {
                ClawError::Backend("the database has no parent directory".to_owned())
            })?,
        ));
        // The persisted pair wins over the config seed — it is what a
        // runtime switch wrote. A persisted id the registry no longer
        // knows (a backend removed from the deployment) falls back to
        // the seed silently: the daemon still boots.
        let persisted_provider = settings.get(crate::settings::ACTIVE_PROVIDER_KEY)?;
        let persisted_model = settings.get(crate::settings::ACTIVE_MODEL_KEY)?;
        if let Some(id) = persisted_provider {
            let _ = backends.set_active(&id, persisted_model.as_deref());
        }
        // The environment report is detection, not conversation: generated
        // at every boot by deterministic discovery, never by the model,
        // and regenerated over whatever the last boot wrote.
        crate::environment::write_report(config.bootstrap_dir()?.join("ENVIRONMENT.md"))?;
        let interactive_handler = InteractiveHandler::new(
            backends.clone(),
            interactive_workspace,
            watches.clone(),
            memory,
            bootstrap.clone(),
            hub.clone(),
            notifications.clone(),
            tasks.clone(),
            accounts.clone(),
        );
        let interactive_driver = Arc::new(
            Driver::new(
                interactive_dispatcher.clone(),
                Arc::new(interactive_handler),
            )
            .with_holder("rhema-claw-interactive"),
        );
        crate::interactive::register_user(&interactive_dispatcher);
        // The scheduler is the resident dispatcher's second own source:
        // its detect pass (the daemon's tick drives it) is what turns a
        // due task row into an event, never a timer near the model.
        let scheduler = TaskScheduler::new(
            &interactive_dispatcher,
            Arc::new(SystemClock),
            tasks.clone(),
        );

        let http = ClawState::new(
            interactive_dispatcher,
            interactive_driver.clone(),
            store,
            watches,
            notifications,
            bootstrap,
            hub,
            backends.clone(),
            settings.clone(),
            identity,
            accounts,
            config.token.clone(),
        );

        Ok(Self {
            sources,
            scheduler,
            forge_driver,
            interactive_driver,
            // A zero poll interval would spin detect passes against the
            // forge API; one second is the floor.
            poll_interval: Duration::from_secs(config.poll_interval_secs.max(1)),
            listen: config.listen_addr().to_owned(),
            http,
            registry: backends,
        })
    }

    /// The HTTP router over the daemon's state — the exact app `run`
    /// serves, exposed so a test can bind it anywhere.
    pub fn router(&self) -> axum::Router {
        crate::http::router(self.http.clone())
    }

    /// The HTTP surface's configured address.
    pub fn listen_addr(&self) -> &str {
        &self.listen
    }

    /// The deployment's backend registry: providers and the active pair.
    pub fn backends(&self) -> &BackendRegistry {
        &self.registry
    }

    /// Startup recovery (RSD spec §7): sweeps leases a dead process left
    /// behind, on both dispatchers. Idempotent; called before the first
    /// pass.
    pub async fn bootstrap(&self) -> Result<(), DispatchError> {
        self.forge_driver.bootstrap().await?;
        self.interactive_driver.bootstrap().await
    }

    /// One pass: every source detects (due tasks first — a reminder is
    /// the user-facing clock), the forge driver runs every key the
    /// debounce released, and the interactive driver drains as a
    /// backstop — a message delivered live into a running turn waits at
    /// the key for a pass like this one.
    pub async fn tick(&self) -> Result<TickReport, ClawError> {
        let mut detected = self.scheduler.detect().await?;
        for source in &self.sources {
            detected += source.detect().await?;
        }
        let outcomes = self.forge_driver.run_once().await?;
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
        match self.interactive_driver.run_once().await {
            Ok(runs) if !runs.is_empty() => {
                eprintln!("[claw] interactive: {} queued run(s) drained", runs.len());
            }
            Ok(_) => {}
            Err(err) => eprintln!("[claw] warning: interactive pass failed: {err}"),
        }
        Ok(TickReport { detected, outcomes })
    }

    /// The resident loop: the HTTP surface comes up first (a resident is
    /// talked to), then detect, run, sleep, until ctrl-c. A failed pass
    /// is logged, not fatal — a forge outage or a dead backend must not
    /// end a daemon whose next pass may simply succeed.
    pub async fn run(&self) -> Result<(), ClawError> {
        self.bootstrap().await?;
        let listener = tokio::net::TcpListener::bind(&self.listen).await?;
        let addr = listener.local_addr()?;
        let app = self.router();
        let server = tokio::spawn(async move {
            if let Err(err) = axum::serve(listener, app).await {
                eprintln!("[claw] http server stopped: {err}");
            }
        });
        eprintln!("[claw] http listening on {addr}");
        loop {
            if let Err(err) = self.tick().await {
                eprintln!("[claw] warning: pass failed: {err}");
            }
            tokio::select! {
                () = tokio::time::sleep(self.poll_interval) => {}
                _ = tokio::signal::ctrl_c() => {
                    server.abort();
                    return Ok(());
                }
            }
        }
    }
}

/// The `--once` shape: assemble over a zero debounce window, recover, run
/// one detect-plus-driver pass, and return. The cron-friendly form for
/// ephemeral machines, and the shape the tests drive. No HTTP: a cron
/// pass has no one to chat with.
pub async fn run_once(
    config: &ClawConfig,
    backends: Arc<BackendRegistry>,
) -> Result<TickReport, ClawError> {
    let dispatch = DispatcherConfig {
        debounce_window_millis: 0,
        ..DispatcherConfig::default()
    };
    let daemon = Daemon::build(config, backends, dispatch)?;
    daemon.bootstrap().await?;
    daemon.tick().await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_interactive_dispatcher_dispatches_now_and_leases_long() {
        let dispatch = interactive_dispatch();
        assert_eq!(dispatch.debounce_window_millis, 0, "chat is immediate");
        assert_eq!(dispatch.lease_ttl_millis, 3_600_000, "an hour");
        assert!(
            dispatch.max_concurrent_leases >= 16,
            "generous: turns are long"
        );
    }
}
