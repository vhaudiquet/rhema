//! The deployable server: env in, routes up.
//!
//! `DATABASE_URL` (the Postgres the checkpoints and configs live in)
//! and `ANTHROPIC_API_KEY` are required; so is `VERCEL_TOKEN` under
//! the default `SANDBOX_BACKEND=vercel`. Under `SANDBOX_BACKEND=http`
//! the Vercel REST API is never called, and `SANDBOX_URL` (the sandbox
//! daemon's base URL) is required instead, with `SANDBOX_TOKEN` the
//! shared secret it checks on every route. `VERCEL_TEAM_ID` and
//! `SANDBOX_API_BASE` are not required. Binds `0.0.0.0` on `PORT`
//! (default 8080). On Vercel, a container runs this binary; locally,
//! `cargo run -p rhema-vercel-agent` serves the same surface.
#![forbid(unsafe_code)]

use rhema_vercel_agent::backend::{
    BACKEND_VAR, SandboxBackend, TOKEN_VAR, URL_VAR, select_backend,
};
use rhema_vercel_agent::{AppState, PgStore, SandboxSource};
use std::sync::Arc;

fn env_required(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| {
        eprintln!("the {name} environment variable is required");
        std::process::exit(2);
    })
}

#[tokio::main]
async fn main() {
    // The backend rides the deployment env, read once here: the
    // sandbox is a deployment decision, and a bad one fails startup
    // with the fix in the message, not the first turn.
    let backend_var = std::env::var(BACKEND_VAR).ok();
    let url_var = std::env::var(URL_VAR).ok();
    let token_var = std::env::var(TOKEN_VAR).ok();
    let backend: SandboxBackend = select_backend(
        backend_var.as_deref(),
        url_var.as_deref(),
        token_var.as_deref(),
    )
    .unwrap_or_else(|message| {
        eprintln!("{message}");
        std::process::exit(2);
    });
    let store = PgStore::open(&env_required("DATABASE_URL")).unwrap_or_else(|err| {
        eprintln!("the database is unreachable: {err}");
        std::process::exit(2);
    });
    let sandbox = SandboxSource::select(backend, || {
        Arc::new(rhema_vercel_agent::ReqwestSandboxApi::new(
            std::env::var("SANDBOX_API_BASE").ok(),
            env_required("VERCEL_TOKEN"),
            std::env::var("VERCEL_TEAM_ID").ok(),
        ))
    });
    let state = Arc::new(AppState::new(
        store,
        sandbox,
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
