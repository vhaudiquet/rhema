//! The Vercel Sandbox REST client, as a seam of its own.
//!
//! [`SandboxApi`] is the REST surface the agent needs — session
//! bootstrap, command execution, file transfer — carried by plain
//! DTOs, with [`ReqwestSandboxApi`] as the real transport. The seam
//! exists for the same reason every other one in this tree does: the
//! offline tests stub it, so the sandbox logic above it
//! ([`VercelSandbox`](crate::VercelSandbox)) is testable without a
//! token or a network.
//!
//! Endpoints, per the Vercel REST reference (api.vercel.com, bearer
//! auth): `GET /v2/sandboxes/{name}?resume=true` resumes or creates a
//! named sandbox and answers with its live session id;
//! `POST /v2/sandboxes` creates one; `POST /v2/sandboxes/sessions/
//! {sessionId}/cmd` runs a command (async — the command id comes back
//! at once, status and logs are polled); `POST .../cmd/{cmdId}/kill`
//! stops one; `POST .../fs/read` and `POST .../fs/write` move files
//! (the write side wants a gzipped tarball and an `x-cwd` header).
#![forbid(unsafe_code)]

use async_trait::async_trait;

/// Where the Vercel REST API lives.
pub const DEFAULT_API_BASE: &str = "https://api.vercel.com";

/// Why a sandbox API call failed. In-band: the shell tool turns every
/// variant into an `is_error` result for the model.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ApiError {
    /// The sandbox does not exist yet — the resume path's signal to
    /// create.
    #[error("sandbox not found: {0}")]
    NotFound(String),
    /// The transport failed, or the answer could not be decoded.
    #[error("sandbox api failed: {0}")]
    Failed(String),
}

/// A live sandbox session: the id every session-scoped call carries.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
pub struct SessionHandle {
    /// The nested `session` object of the REST answers.
    #[serde(rename = "session")]
    pub inner: SessionBody,
}

/// The fields of a session answer the agent reads.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
pub struct SessionBody {
    /// The unique identifier of the sandbox session.
    pub id: String,
}

/// One command to execute: the REST body of `POST .../cmd`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CmdRequest {
    /// The program to run.
    pub command: String,
    /// Its arguments, one per element.
    pub args: Vec<String>,
    /// The working directory, absolute.
    pub cwd: String,
    /// Kill budget in milliseconds; the sandbox SIGKILLs past it.
    pub timeout: u64,
}

/// The command object the REST answers carry.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CmdStatus {
    /// The command's id.
    pub id: String,
    /// The exit code, until the command finishes.
    pub exit_code: Option<i32>,
}

/// The `POST .../cmd` answer: the command, started.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
pub struct CmdStarted {
    /// The nested `command` object.
    #[serde(rename = "command")]
    pub inner: CmdStatus,
}

/// The `GET .../cmd/{cmdId}` answer.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
pub struct CmdAnswer {
    /// The nested `command` object.
    #[serde(rename = "command")]
    pub inner: CmdStatus,
}

/// One NDJSON line of the command logs endpoint: which stream, and the
/// text that landed on it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
pub struct LogEntry {
    /// `stdout` or `stderr`.
    pub stream: String,
    /// The chunk of output.
    pub data: String,
}

/// The REST surface the agent's sandbox needs.
#[async_trait]
pub trait SandboxApi: Send + Sync {
    /// `GET /v2/sandboxes/{name}?resume=true`: the named sandbox's
    /// live session, resumed when it was stopped.
    async fn named_session(&self, name: &str) -> Result<SessionHandle, ApiError>;

    /// `POST /v2/sandboxes`: create a named, persistent sandbox and
    /// answer with its first session.
    async fn create_named(&self, name: &str) -> Result<SessionHandle, ApiError>;

    /// `POST .../cmd`: start a command, async — the id comes back at
    /// once and status arrives by polling [`Self::command`].
    async fn exec(&self, session: &str, request: &CmdRequest) -> Result<CmdStarted, ApiError>;

    /// `GET .../cmd/{cmdId}`: one command's status.
    async fn command(&self, session: &str, cmd: &str) -> Result<CmdAnswer, ApiError>;

    /// `POST .../cmd/{cmdId}/kill`: stop one running command.
    async fn kill(&self, session: &str, cmd: &str) -> Result<(), ApiError>;

    /// `GET .../cmd/{cmdId}/logs`: the command's output as NDJSON.
    async fn logs(&self, session: &str, cmd: &str) -> Result<Vec<LogEntry>, ApiError>;

    /// `POST .../fs/read`: one file's bytes.
    async fn read_file(&self, session: &str, path: &str) -> Result<Vec<u8>, ApiError>;

    /// `POST .../fs/write`: upload a gzipped tarball, extracted at
    /// `cwd` (the `x-cwd` header).
    async fn write_files(&self, session: &str, cwd: &str, tar_gz: Vec<u8>) -> Result<(), ApiError>;
}

/// The real transport: reqwest against `api.vercel.com`, bearer token,
/// optional team id.
pub struct ReqwestSandboxApi {
    http: reqwest::Client,
    base: String,
    token: String,
    team: Option<String>,
}

impl ReqwestSandboxApi {
    /// A client for `base` (or [`DEFAULT_API_BASE`]) with `token` and,
    /// when the account uses teams, `team`.
    pub fn new(base: Option<String>, token: String, team: Option<String>) -> Self {
        Self {
            http: reqwest::Client::new(),
            base: base.unwrap_or_else(|| DEFAULT_API_BASE.to_owned()),
            token,
            team,
        }
    }

    /// The URL of one path under the API base.
    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    /// The query string with the team parameter, when set.
    fn team_query(&self) -> String {
        match &self.team {
            Some(team) => format!("?teamId={team}"),
            None => String::new(),
        }
    }

    /// A GET with the bearer header.
    async fn get(&self, path: &str) -> Result<reqwest::Response, ApiError> {
        self.http
            .get(self.url(path))
            .query(&[("resume", "true")])
            .header("Authorization", format!("Bearer {}", self.token))
            .send()
            .await
            .map_err(|err| ApiError::Failed(err.to_string()))
    }
}

#[async_trait]
impl SandboxApi for ReqwestSandboxApi {
    async fn named_session(&self, name: &str) -> Result<SessionHandle, ApiError> {
        let response = self.get(&format!("/v2/sandboxes/{name}")).await?;
        match response.status() {
            reqwest::StatusCode::OK => response
                .json()
                .await
                .map_err(|err| ApiError::Failed(format!("malformed session: {err}"))),
            reqwest::StatusCode::NOT_FOUND => Err(ApiError::NotFound(name.to_owned())),
            status => Err(ApiError::Failed(format!("named session: {status}"))),
        }
    }

    async fn create_named(&self, name: &str) -> Result<SessionHandle, ApiError> {
        let response = self
            .http
            .post(self.url("/v2/sandboxes") + &self.team_query())
            .header("Authorization", format!("Bearer {}", self.token))
            .json(&serde_json::json!({ "name": name, "persistent": true }))
            .send()
            .await
            .map_err(|err| ApiError::Failed(err.to_string()))?;
        match response.status() {
            reqwest::StatusCode::OK => response
                .json()
                .await
                .map_err(|err| ApiError::Failed(format!("malformed session: {err}"))),
            status => Err(ApiError::Failed(format!("create sandbox: {status}"))),
        }
    }

    async fn exec(&self, session: &str, request: &CmdRequest) -> Result<CmdStarted, ApiError> {
        let response = self
            .http
            .post(self.url(&format!("/v2/sandboxes/sessions/{session}/cmd")) + &self.team_query())
            .header("Authorization", format!("Bearer {}", self.token))
            .json(request)
            .send()
            .await
            .map_err(|err| ApiError::Failed(err.to_string()))?;
        if !response.status().is_success() {
            return Err(ApiError::Failed(format!("exec: {}", response.status())));
        }
        response
            .json()
            .await
            .map_err(|err| ApiError::Failed(format!("malformed command: {err}")))
    }

    async fn command(&self, session: &str, cmd: &str) -> Result<CmdAnswer, ApiError> {
        let response = self
            .http
            .get(
                self.url(&format!("/v2/sandboxes/sessions/{session}/cmd/{cmd}"))
                    + &self.team_query(),
            )
            .header("Authorization", format!("Bearer {}", self.token))
            .send()
            .await
            .map_err(|err| ApiError::Failed(err.to_string()))?;
        if !response.status().is_success() {
            return Err(ApiError::Failed(format!("command: {}", response.status())));
        }
        response
            .json()
            .await
            .map_err(|err| ApiError::Failed(format!("malformed command: {err}")))
    }

    async fn kill(&self, session: &str, cmd: &str) -> Result<(), ApiError> {
        let response = self
            .http
            .post(
                self.url(&format!("/v2/sandboxes/sessions/{session}/cmd/{cmd}/kill"))
                    + &self.team_query(),
            )
            .header("Authorization", format!("Bearer {}", self.token))
            .send()
            .await
            .map_err(|err| ApiError::Failed(err.to_string()))?;
        if !response.status().is_success() {
            return Err(ApiError::Failed(format!("kill: {}", response.status())));
        }
        Ok(())
    }

    async fn logs(&self, session: &str, cmd: &str) -> Result<Vec<LogEntry>, ApiError> {
        let response = self
            .http
            .get(
                self.url(&format!("/v2/sandboxes/sessions/{session}/cmd/{cmd}/logs"))
                    + &self.team_query(),
            )
            .header("Authorization", format!("Bearer {}", self.token))
            .send()
            .await
            .map_err(|err| ApiError::Failed(err.to_string()))?;
        if !response.status().is_success() {
            return Err(ApiError::Failed(format!("logs: {}", response.status())));
        }
        let text = response
            .text()
            .await
            .map_err(|err| ApiError::Failed(format!("unreadable logs: {err}")))?;
        Ok(text
            .lines()
            .filter(|line| !line.trim().is_empty())
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect())
    }

    async fn read_file(&self, session: &str, path: &str) -> Result<Vec<u8>, ApiError> {
        let response = self
            .http
            .post(
                self.url(&format!("/v2/sandboxes/sessions/{session}/fs/read")) + &self.team_query(),
            )
            .header("Authorization", format!("Bearer {}", self.token))
            .json(&serde_json::json!({ "path": path, "cwd": "/" }))
            .send()
            .await
            .map_err(|err| ApiError::Failed(err.to_string()))?;
        match response.status() {
            reqwest::StatusCode::OK => response
                .bytes()
                .await
                .map(|bytes| bytes.to_vec())
                .map_err(|err| ApiError::Failed(format!("unreadable file: {err}"))),
            reqwest::StatusCode::NOT_FOUND => Err(ApiError::NotFound(path.to_owned())),
            status => Err(ApiError::Failed(format!("read file: {status}"))),
        }
    }

    async fn write_files(&self, session: &str, cwd: &str, tar_gz: Vec<u8>) -> Result<(), ApiError> {
        let response = self
            .http
            .post(
                self.url(&format!("/v2/sandboxes/sessions/{session}/fs/write"))
                    + &self.team_query(),
            )
            .header("Authorization", format!("Bearer {}", self.token))
            .header("x-cwd", cwd)
            .header("Content-Type", "application/gzip")
            .body(tar_gz)
            .send()
            .await
            .map_err(|err| ApiError::Failed(err.to_string()))?;
        if !response.status().is_success() {
            return Err(ApiError::Failed(format!(
                "write files: {}",
                response.status()
            )));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_log_line_deserializes_from_ndjson() {
        let entry: LogEntry =
            serde_json::from_str(r#"{"stream":"stdout","data":"hello\n"}"#).unwrap();
        assert_eq!(entry.stream, "stdout");
        assert_eq!(entry.data, "hello\n");
    }

    #[test]
    fn the_cmd_request_serializes_in_camel_case() {
        let request = CmdRequest {
            command: "bash".into(),
            args: vec!["-c".into(), "ls".into()],
            cwd: "/workspace".into(),
            timeout: 5_000,
        };
        let json = serde_json::to_value(&request).unwrap();
        assert_eq!(json["timeout"], 5_000, "the budget rides in milliseconds");
        assert!(json.get("timeout_ms").is_none());
    }
}
