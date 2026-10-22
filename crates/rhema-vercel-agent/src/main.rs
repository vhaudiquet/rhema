//! The deployable server: env in, routes up.
//!
//! `DATABASE_URL` (the Postgres the checkpoints and configs live in),
//! `ANTHROPIC_API_KEY`, and `VERCEL_TOKEN` are required;
//! `VERCEL_TEAM_ID` and `SANDBOX_API_BASE` are not. Binds `0.0.0.0`
//! on `PORT` (default 8080). On Vercel, a container runs this binary;
//! locally, `cargo run -p rhema-vercel-agent` serves the same surface.
#![forbid(unsafe_code)]

use rhema_vercel_agent::{AppState, PgStore};
use std::sync::Arc;

fn env_required(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| {
        eprintln!("the {name} environment variable is required");
        std::process::exit(2);
    })
}

#[tokio::main]
async fn main() {
    let store = PgStore::open(&env_required("DATABASE_URL")).unwrap_or_else(|err| {
        eprintln!("the database is unreachable: {err}");
        std::process::exit(2);
    });
    let sandbox_api = Arc::new(rhema_vercel_agent::ReqwestSandboxApi::new(
        std::env::var("SANDBOX_API_BASE").ok(),
        env_required("VERCEL_TOKEN"),
        std::env::var("VERCEL_TEAM_ID").ok(),
    ));
    let state = Arc::new(AppState::new(
        store,
        sandbox_api,
        env_required("ANTHROPIC_API_KEY"),
    ));

    let port: u16 = std::env::var("PORT")
        .ok()
        .and_then(|port| port.parse().ok())
        .unwrap_or(8080);
    let listener = tokio::net::TcpListener::bind(("0.0.0.0", port))
        .await
        .expect("the port binds");
    println!("rhema-vercel-agent listening on {port}");
    axum::serve(listener, rhema_vercel_agent::router(state))
        .await
        .expect("the server serves");
}
