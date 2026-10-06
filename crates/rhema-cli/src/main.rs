//! Argument wiring only; the REPL and rendering live in the library so
//! they stay testable.

#![forbid(unsafe_code)]

use clap::Parser;

use rhema_cli::Args;

#[tokio::main]
async fn main() {
    let args = Args::parse();
    if let Err(err) = rhema_cli::run(args).await {
        // One clean line on stderr; non-zero exit. REPL turn errors are
        // handled inside `run` and never reach here.
        eprintln!("error: {err:#}");
        std::process::exit(1);
    }
}
