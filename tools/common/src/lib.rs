//! Shared plumbing for the claw's thin client tools. A client is three
//! moves: read the connection from the environment, make one HTTP call
//! carrying the contract header, and print either the answer as one
//! JSON line or the one error shape — then exit with the code that
//! says which happened. This crate is those three moves, so no client
//! reimplements them and all clients fail identically.

use std::io::Write;

use rhema_tools_contract::{ApiError, CONTRACT_HEADER, CONTRACT_VERSION};
use serde::Serialize;
use serde_json::Value;

/// The default daemon address: the claw's default listen port, spoken
/// over loopback. `RHEMA_URL` overrides.
pub const DEFAULT_BASE_URL: &str = "http://127.0.0.1:7799";

/// Where the connection comes from: `RHEMA_URL` (the daemon's base
/// address, defaulting to [`DEFAULT_BASE_URL`]) and `RHEMA_TOKEN` (the
/// scoped agent token the daemon provisions). No token means the
/// daemon's surface is open; the client still sends it as a header
/// when present.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// The daemon's base address, no trailing slash.
    pub base_url: String,
    /// The agent token, when the daemon issued one.
    pub token: Option<String>,
}

impl Config {
    /// Reads the environment: `RHEMA_URL`, `RHEMA_TOKEN`.
    pub fn from_env() -> Self {
        let base_url = std::env::var("RHEMA_URL")
            .ok()
            .filter(|url| !url.is_empty())
            .unwrap_or_else(|| DEFAULT_BASE_URL.to_owned());
        let token = std::env::var("RHEMA_TOKEN").ok().filter(|t| !t.is_empty());
        Self { base_url, token }
    }
}

/// How a client call failed. The variant is the exit code's meaning:
/// usage problems never reach here (clap owns them, exit 1), the
/// daemon being gone is 2, being refused is 3, and a daemon-side
/// rejection is 1 like any other failure — the printed error line
/// carries the nuance.
#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    /// The daemon could not be reached at all.
    #[error("cannot reach the daemon at {0}: check RHEMA_URL and that the claw is running")]
    Unreachable(String),
    /// The daemon refused the credential or the contract version.
    #[error("the daemon refused the call: {0}")]
    Denied(String),
    /// The daemon answered with an error.
    #[error("{0}")]
    Api(String),
}

impl ToolError {
    /// The process exit code for this failure: 2 unreachable, 3
    /// denied, 1 everything else.
    pub fn exit_code(&self) -> i32 {
        match self {
            ToolError::Unreachable(_) => 2,
            ToolError::Denied(_) => 3,
            ToolError::Api(_) => 1,
        }
    }
}

/// A configured client over the contract: one struct per binary, one
/// call per invocation.
pub struct Client {
    config: Config,
    agent: ureq::Agent,
}

impl Client {
    /// A client on the environment's connection.
    pub fn from_env() -> Self {
        Self::new(Config::from_env())
    }

    /// A client on an explicit connection — tests mostly.
    pub fn new(config: Config) -> Self {
        Self {
            config,
            agent: ureq::AgentBuilder::new().build(),
        }
    }

    /// Performs one contract call and answers the daemon's JSON. Every
    /// failure mode prints through [`fail`]; nothing here writes to
    /// stderr, the error shape is stdout's last line by contract.
    pub fn call(
        &self,
        method: &str,
        path: &str,
        body: Option<&impl Serialize>,
    ) -> Result<Value, ToolError> {
        let url = format!("{}{path}", self.config.base_url);
        let mut request = self
            .agent
            .request(method, &url)
            .set(CONTRACT_HEADER, &CONTRACT_VERSION.to_string());
        if let Some(token) = &self.config.token {
            request = request.set("Authorization", &format!("Bearer {token}"));
        }
        let response = match body {
            Some(body) => request.send_json(body),
            None => request.call(),
        };
        let response = match response {
            Ok(response) => response,
            Err(ureq::Error::Status(_, response)) => {
                return Err(Self::error_response(response));
            }
            Err(_) => {
                return Err(ToolError::Unreachable(self.config.base_url.clone()));
            }
        };
        let status = response.status();
        if !(200..300).contains(&status) {
            return Err(Self::error_response(response));
        }
        serde_json::from_reader(response.into_reader())
            .map_err(|err| ToolError::Api(format!("the daemon's answer does not parse: {err}")))
    }

    /// Reads a non-2xx answer into the error it speaks: the one shape,
    /// with the status class picking the variant.
    fn error_response(response: ureq::Response) -> ToolError {
        let status = response.status();
        let message = serde_json::from_reader::<_, ApiError>(response.into_reader())
            .map(|err| err.error)
            .unwrap_or_else(|_| format!("HTTP {status}"));
        match status {
            401 | 403 | 409 => ToolError::Denied(message),
            _ => ToolError::Api(message),
        }
    }
}

/// Prints one answer as one JSON line — the contract's stdout.
pub fn print(value: &Value) {
    let line = serde_json::to_string(value).expect("a JSON value prints");
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{line}");
}

/// Prints the error shape and exits with the variant's code. The one
/// exit path every client shares.
pub fn fail(error: ToolError) -> ! {
    let line = serde_json::to_string(&ApiError {
        error: error.to_string(),
    })
    .expect("an error shape prints");
    println!("{line}");
    std::process::exit(error.exit_code());
}
