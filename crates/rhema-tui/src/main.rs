//! Binary wiring only: parse args, run the TUI, report one error line.
//! Terminal setup and teardown live in `rhema_tui::run`.

#![forbid(unsafe_code)]

use clap::Parser;

use rhema_tui::Args;

#[tokio::main]
async fn main() {
    let args = Args::parse();
    if let Err(err) = rhema_tui::run(args).await {
        // One clean line on stderr; non-zero exit. The terminal is already
        // restored by the time `run` returns.
        eprintln!("error: {err:#}");
        std::process::exit(1);
    }
}
