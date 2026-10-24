//! Binary wiring only: parse the args, bind, serve. The daemon, its
//! routes, and the run loop live in `rhema_sandbox_server`.

#![forbid(unsafe_code)]

use clap::Parser;
use rhema_sandbox_server::{Args, run};

#[tokio::main]
async fn main() {
    let args = Args::parse();
    if let Err(err) = run(args).await {
        eprintln!("error: {err}");
        std::process::exit(1);
    }
}
