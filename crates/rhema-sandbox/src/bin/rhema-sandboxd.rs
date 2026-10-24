//! Binary wiring only: parse the args, bind, serve. The daemon, its
//! routes, and the run loop live in `rhema_sandbox::server`.

#![forbid(unsafe_code)]

use clap::Parser;
use rhema_sandbox::server::{self, Args};

#[tokio::main]
async fn main() {
    let args = Args::parse();
    if let Err(err) = server::run(args).await {
        eprintln!("error: {err}");
        std::process::exit(1);
    }
}
