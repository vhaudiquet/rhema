//! Binary wiring only: parse the two args, resolve the model backend from
//! the stored rhema credentials, run the daemon. The loop and its
//! assembly live in `rhema_claw::daemon`.

#![forbid(unsafe_code)]

use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;
use rhema_claw::{ClawConfig, ClawError, Daemon, run_once};
use rhema_core::ModelProvider;
use rhema_provider_anthropic::AnthropicProvider;
use rhema_provider_google::GoogleProvider;
use rhema_provider_llama_cpp::LlamaCppProvider;
use rhema_provider_openai_compatible::OpenAiCompatibleProvider;
use rhema_provider_openrouter::OpenRouterProvider;
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
    let (provider, model) = if args.mock {
        mock_backend()
    } else {
        resolve_backend(&config)
    };
    eprintln!(
        "[claw] {} seeded watch(es), model {model}, db {}, http {}",
        config.watches.len(),
        config
            .db_path()
            .map(|db| db.display().to_string())
            .unwrap_or_default(),
        config.listen_addr(),
    );

    if args.once {
        let report = run_once(&config, provider).await?;
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
        // how it is talked to. Only `--once` runs headless.
        let daemon = Daemon::build(&config, provider, DispatcherConfig::default())?;
        daemon.run().await
    }
}

/// The `--mock` backend: an endless canned provider behind the `mock`
/// feature; without it the flag is a loud startup failure naming the fix.
#[cfg(feature = "mock")]
fn mock_backend() -> (Arc<dyn ModelProvider>, String) {
    eprintln!("[claw] mock backend: every turn answers with canned streamed text");
    (rhema_claw::mock::MockLoop::provider(), "mock".to_owned())
}

#[cfg(not(feature = "mock"))]
fn mock_backend() -> (Arc<dyn ModelProvider>, String) {
    eprintln!("error: --mock needs a mock build: cargo build -p rhema-claw --features mock");
    std::process::exit(1);
}

/// The handler's backend, resolved through the stored rhema credentials
/// exactly as the frontend resolves them: the env key wins, the stored
/// key serves, and a backend without its key is a startup failure — a
/// daemon must not boot to fail on every dispatch.
fn resolve_backend(config: &ClawConfig) -> (Arc<dyn ModelProvider>, String) {
    let stored = match rhema_config::Config::load() {
        Ok(stored) => stored,
        Err(err) => {
            eprintln!("warning: ignoring unreadable rhema config: {err}");
            rhema_config::Config::default()
        }
    };
    let profile = stored.effective(None);
    let provider: Arc<dyn ModelProvider> = match config.provider.as_deref().unwrap_or("google") {
        "google" => Arc::new(GoogleProvider::new(
            profile
                .google_api_key()
                .unwrap_or_else(|| die("google", "GEMINI_API_KEY")),
        )),
        "llama-cpp" => {
            // A llama.cpp server works keyless; the bearer key is
            // optional, the URL defaults to localhost.
            let mut provider = LlamaCppProvider::new(profile.llama_cpp_url());
            if let Some(key) = profile.llama_cpp_api_key() {
                provider = provider.with_api_key(key);
            }
            Arc::new(provider)
        }
        "openrouter" => Arc::new(OpenRouterProvider::new(
            profile
                .openrouter_api_key()
                .unwrap_or_else(|| die("openrouter", "OPENROUTER_API_KEY")),
        )),
        "anthropic" => Arc::new(AnthropicProvider::new(
            profile
                .anthropic_api_key()
                .unwrap_or_else(|| die("anthropic", "ANTHROPIC_API_KEY")),
        )),
        "openai" => Arc::new(OpenAiCompatibleProvider::new(
            profile
                .openai_api_key()
                .unwrap_or_else(|| die("openai", "OPENAI_API_KEY")),
        )),
        other => {
            eprintln!(
                "error: unknown provider {other:?}: google, llama-cpp, openrouter, anthropic, or openai"
            );
            std::process::exit(1);
        }
    };
    let model = config.resolved_model();
    (provider, model)
}

/// Exits naming the provider and the env variable that would fix it.
fn die(provider: &str, env: &str) -> ! {
    eprintln!(
        "error: missing API key for provider {provider}: set {env} (or store one with rhema)"
    );
    std::process::exit(1);
}
