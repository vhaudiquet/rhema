//! Binary wiring only: parse the two args, resolve the model backend from
//! the stored rhema credentials, run the daemon. The loop and its
//! assembly live in `rhema_claw::daemon`.

#![forbid(unsafe_code)]

use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;
use rhema_claw::{ClawConfig, ClawError, Daemon, run_once};
use rhema_rsd::{DispatcherConfig, Outcome};

/// `rhema-claw` — the resident forge-watching agent daemon.
#[derive(Debug, Parser)]
#[command(
    name = "rhema-claw",
    version,
    about = "Resident forge-watching agent daemon"
)]
struct Args {
    /// Path to the claw config. Defaults to ~/.config/rhema/claw.yaml.
    #[arg(long)]
    config: Option<PathBuf>,

    /// Run one detect pass and one driver pass, then exit — the
    /// cron-friendly shape for ephemeral machines.
    #[arg(long)]
    once: bool,

    /// Answer every turn with canned streamed text: the zero-credential
    /// debug backend. Requires the `mock` feature.
    #[arg(long)]
    mock: bool,
}

#[tokio::main]
async fn main() {
    if let Err(err) = run(Args::parse()).await {
        eprintln!("error: {err}");
        let mut source: &dyn std::error::Error = &err;
        while let Some(cause) = source.source() {
            eprintln!("  caused by: {cause}");
            source = cause;
        }
        std::process::exit(1);
    }
}

async fn run(args: Args) -> Result<(), ClawError> {
    let path = match args.config {
        Some(path) => path,
        None => ClawConfig::default_path()?,
    };
    let config = ClawConfig::load_from(&path)?;
    let backends = if args.mock {
        mock_backends()
    } else {
        resolve_backends(&config)?
    };
    eprintln!(
        "[claw] {} seeded watch(es), active {}/{}, db {}, http {}",
        config.watches.len(),
        backends.active().0,
        backends.active().1,
        config
            .db_path()
            .map(|db| db.display().to_string())
            .unwrap_or_default(),
        config.listen_addr(),
    );

    if args.once {
        let report = run_once(&config, backends.clone()).await?;
        // A cron pass reports its failures through the exit code.
        if report
            .outcomes
            .iter()
            .any(|outcome| matches!(outcome, Outcome::Failed(_)))
        {
            std::process::exit(1);
        }
        Ok(())
    } else {
        // The resident daemon always serves HTTP: the chat surface is
        // how it is talked to. Only `--once` runs headless. The
        // surface's posture is checked before anything binds: a wide
        // listen with no token never comes up.
        config.validate_surface()?;
        let daemon = Daemon::build(&config, backends, DispatcherConfig::default())?;
        daemon.run().await
    }
}

/// The `--mock` backend: an endless canned provider behind the `mock`
/// feature; without it the flag is a loud startup failure naming the fix.
#[cfg(feature = "mock")]
fn mock_backends() -> Arc<rhema_claw::BackendRegistry> {
    eprintln!("[claw] mock backend: every turn answers with canned streamed text");
    Arc::new(
        rhema_claw::BackendRegistry::new(
            vec![rhema_claw::Backend::headless(
                "mock",
                rhema_claw::mock::MockLoop::provider(),
            )],
            None,
        )
        .expect("a single mock backend always constructs"),
    )
}

#[cfg(not(feature = "mock"))]
fn mock_backends() -> Arc<rhema_claw::BackendRegistry> {
    eprintln!("error: --mock needs a mock build: cargo build -p rhema-claw --features mock");
    std::process::exit(1);
}

/// The deployment's backends: every provider whose credentials resolve
/// (env wins, the stored rhema config serves), with the config's
/// provider/model pair as the active seed. A daemon always boots —
/// the keyless llama-cpp backend is always constructible — and a
/// dispatch against an unreachable provider fails loudly per turn.
fn resolve_backends(config: &ClawConfig) -> Result<Arc<rhema_claw::BackendRegistry>, ClawError> {
    let stored = match rhema_config::Config::load() {
        Ok(stored) => stored,
        Err(err) => {
            eprintln!("warning: ignoring unreadable rhema config: {err}");
            rhema_config::Config::default()
        }
    };
    let profile = stored.effective(None);
    // A second connection to the shared database, read-only in spirit:
    // discovery reads the credentials the HTTP surface persisted. An
    // unopenable store boots the daemon without them rather than not at
    // all — the same grace the rhema config gets above.
    let claw_creds = rhema_claw::SettingsStore::open(config.db_path()?).ok();
    let claw_rows = move || {
        claw_creds
            .as_ref()
            .and_then(|store| {
                store
                    .get_prefix(rhema_claw::backends::PROVIDER_KEY_PREFIX)
                    .ok()
            })
            .unwrap_or_default()
            .into_iter()
            .filter_map(|(key, json)| {
                let id = key.strip_prefix(rhema_claw::backends::PROVIDER_KEY_PREFIX)?;
                serde_json::from_str(&json)
                    .ok()
                    .map(|params| (id.to_owned(), params))
            })
            .collect::<Vec<_>>()
    };
    let env = |name: &str| std::env::var(name).ok();
    let backends = rhema_claw::backends::discover(config, &profile, &env, &claw_rows);
    let seed = config
        .provider
        .as_deref()
        .map(|kind| (kind.to_owned(), config.model.clone()));
    Ok(Arc::new(
        rhema_claw::BackendRegistry::new(backends, seed).map_err(ClawError::Backend)?,
    ))
}
