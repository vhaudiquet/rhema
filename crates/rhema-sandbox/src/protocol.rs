//! The sandbox wire protocol: the JSON shapes, the routes, and the auth
//! header every sandbox backend speaks.
//!
//! The shapes are `deploy/cloudflare/sandbox-shim.mjs`'s documented
//! surface, promoted: one command over [`EXEC`], single files over
//! [`PUT`] and [`GET`], a workspace seed over [`SEED`] — every route a
//! JSON POST carrying the shared secret in [`TOKEN_HEADER`]. No version
//! field in v1: a shape drift fails a test here instead of a debugging
//! session there. Caps are per-implementation policy, not protocol —
//! stream and payload size limits are imposed by each side before the
//! bytes cross, so they belong to the implementations, not to these
//! types.
#![forbid(unsafe_code)]

use serde::{Deserialize, Serialize};

/// Runs one command: [`ExecCall`] in, [`ExecReply`] out.
pub const EXEC: &str = "/exec";
/// Writes one file: [`PutFile`] in, [`Ack`] out.
pub const PUT: &str = "/put";
/// Reads one file: `{ "path": ... }` in, [`FileReply`] out.
pub const GET: &str = "/get";
/// Seeds a fresh workspace: [`SeedRequest`] in, [`Ack`] out.
pub const SEED: &str = "/seed";
/// The header carrying the shared secret on every route; a wrong or
/// missing token is refused before the route runs.
pub const TOKEN_HEADER: &str = "x-rhema-token";

/// One command to run: the request body of [`EXEC`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecCall {
    /// The command line, run with `bash -c`.
    pub command: String,
    /// Working directory inside the sandbox, POSIX-style (`/workspace`);
    /// absent means the sandbox's own default. A string, not a
    /// `PathBuf`: the path is meaningful only inside the sandbox's
    /// filesystem, and no host should be tempted to touch it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// Wall-clock budget in seconds; the sandbox kills the command —
    /// and every process it started — past it.
    pub timeout_secs: u64,
}

/// What a command left behind: the reply body of [`EXEC`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecReply {
    /// The exit code; `None` when the process died by signal.
    pub exit_code: Option<i32>,
    /// Everything the command wrote to stdout, tail-capped by the
    /// sandbox before the reply crosses the wire.
    pub stdout: String,
    /// Everything the command wrote to stderr, tail-capped likewise.
    pub stderr: String,
    /// Whether the sandbox's own timeout killed the command; the client
    /// maps this to a timeout error rather than a bare exit code.
    pub timed_out: bool,
}

/// One file to write: the request body of [`PUT`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PutFile {
    /// Sandbox-absolute POSIX path to write (or replace); missing
    /// parent directories are the sandbox's business.
    pub path: String,
    /// The file's bytes, base64.
    pub content_base64: String,
}

/// One file's contents: the reply body of [`GET`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileReply {
    /// The file's bytes, base64; `None` when the read failed.
    #[serde(default)]
    pub content_base64: Option<String>,
    /// The failure message; `None` on success.
    #[serde(default)]
    pub error: Option<String>,
}

/// The reply of [`PUT`] and [`SEED`]: `error` null on success, the
/// failure message otherwise.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ack {
    /// What went wrong; absent and null both mean success.
    #[serde(default)]
    pub error: Option<String>,
}

/// How a fresh workspace gets its contents: the request body of
/// [`SEED`], tagged by field presence the way the shim reads it —
/// `{ "git": url }` or `{ "tarball_base64": ..., "tarball_name": ... }`.
/// Untagged, because the shim dispatches on truthiness, not on a type
/// key, so the wire carries exactly the two bodies and nothing else.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SeedRequest {
    /// `git clone <url> .` into the workspace root — the repository
    /// becomes the workspace.
    Git {
        /// The repository URL to clone.
        git: String,
    },
    /// A gzipped tarball, base64-encoded, unpacked at the workspace
    /// root.
    Tarball {
        /// The tarball's bytes, base64.
        tarball_base64: String,
        /// A file name for the upload, diagnostics only; the shim never
        /// reads it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tarball_name: Option<String>,
    },
}

/// Streams a reply's output through `progress`: stdout's lines first,
/// then stderr's, each with a trailing newline.
///
/// The v1 shim buffers the whole (already tail-capped) output, so this
/// is one burst, not live lines — the callback shape is the contract a
/// streaming shim fills in.
pub fn stream_reply(reply: &ExecReply, progress: &mut (dyn for<'a> FnMut(&'a str) + Send)) {
    for line in reply.stdout.lines() {
        progress(&format!("{line}\n"));
    }
    for line in reply.stderr.lines() {
        progress(&format!("{line}\n"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_exec_call_round_trips_with_a_cwd() {
        let wire = r#"{"command":"cargo test","cwd":"/workspace","timeout_secs":120}"#;
        let call: ExecCall = serde_json::from_str(wire).unwrap();
        assert_eq!(call.command, "cargo test");
        assert_eq!(call.cwd.as_deref(), Some("/workspace"));
        assert_eq!(call.timeout_secs, 120);
        assert_eq!(
            serde_json::to_value(&call).unwrap(),
            serde_json::from_str::<serde_json::Value>(wire).unwrap(),
        );
    }

    #[test]
    fn an_exec_call_without_a_cwd_omits_the_key() {
        let call: ExecCall = serde_json::from_str(r#"{"command":":","timeout_secs":1}"#).unwrap();
        assert_eq!(call.cwd, None);
        assert_eq!(
            serde_json::to_value(&call).unwrap(),
            serde_json::json!({ "command": ":", "timeout_secs": 1 }),
        );
    }

    #[test]
    fn an_exec_reply_round_trips() {
        let wire = r#"{"exit_code":0,"stdout":"hi\n","stderr":"","timed_out":false}"#;
        let reply: ExecReply = serde_json::from_str(wire).unwrap();
        assert_eq!(reply.exit_code, Some(0));
        assert!(!reply.timed_out);
        assert_eq!(
            serde_json::to_value(&reply).unwrap(),
            serde_json::from_str::<serde_json::Value>(wire).unwrap(),
        );
    }

    #[test]
    fn a_timed_out_reply_carries_the_kill_code() {
        // The shim's `timeout --kill-after` lands as SIGKILL, which the
        // shell reports as 137.
        let wire = r#"{"exit_code":137,"stdout":"partial\n","stderr":"","timed_out":true}"#;
        let reply: ExecReply = serde_json::from_str(wire).unwrap();
        assert!(reply.timed_out);
        assert_eq!(
            serde_json::to_value(&reply).unwrap(),
            serde_json::from_str::<serde_json::Value>(wire).unwrap(),
        );
    }

    #[test]
    fn a_null_exit_code_deserializes() {
        let reply: ExecReply =
            serde_json::from_str(r#"{"exit_code":null,"stdout":"","stderr":"","timed_out":false}"#)
                .unwrap();
        assert_eq!(reply.exit_code, None);
    }

    #[test]
    fn a_put_file_round_trips() {
        let wire = r#"{"path":"/workspace/SYSTEM.md","content_base64":"aGk="}"#;
        let put: PutFile = serde_json::from_str(wire).unwrap();
        assert_eq!(put.path, "/workspace/SYSTEM.md");
        assert_eq!(put.content_base64, "aGk=");
        assert_eq!(
            serde_json::to_value(&put).unwrap(),
            serde_json::from_str::<serde_json::Value>(wire).unwrap(),
        );
    }

    #[test]
    fn a_file_reply_round_trips() {
        let wire = r#"{"content_base64":"aGk=","error":null}"#;
        let reply: FileReply = serde_json::from_str(wire).unwrap();
        assert_eq!(reply.content_base64.as_deref(), Some("aGk="));
        assert_eq!(reply.error, None);
        assert_eq!(
            serde_json::to_value(&reply).unwrap(),
            serde_json::from_str::<serde_json::Value>(wire).unwrap(),
        );
    }

    #[test]
    fn an_all_null_file_reply_round_trips() {
        let wire = r#"{"content_base64":null,"error":null}"#;
        let reply: FileReply = serde_json::from_str(wire).unwrap();
        assert_eq!(reply.content_base64, None);
        assert_eq!(reply.error, None);
        assert_eq!(
            serde_json::to_value(&reply).unwrap(),
            serde_json::json!({ "content_base64": null, "error": null }),
        );
    }

    #[test]
    fn the_missing_file_shape_deserializes() {
        // The shim's 404 body carries the error only.
        let reply: FileReply = serde_json::from_str(r#"{"error":"no such file"}"#).unwrap();
        assert_eq!(reply.content_base64, None);
        assert_eq!(reply.error.as_deref(), Some("no such file"));
    }

    #[test]
    fn a_successful_ack_is_a_null_error() {
        let ack: Ack = serde_json::from_str(r#"{"error":null}"#).unwrap();
        assert_eq!(ack.error, None);
        assert_eq!(
            serde_json::to_value(&ack).unwrap(),
            serde_json::json!({ "error": null }),
        );
    }

    #[test]
    fn a_failed_ack_carries_the_message() {
        let ack: Ack = serde_json::from_str(r#"{"error":"clone failed"}"#).unwrap();
        assert_eq!(ack.error.as_deref(), Some("clone failed"));
        assert_eq!(
            serde_json::to_value(&ack).unwrap(),
            serde_json::json!({ "error": "clone failed" }),
        );
    }

    #[test]
    fn an_ack_without_an_error_key_is_a_success() {
        let ack: Ack = serde_json::from_str("{}").unwrap();
        assert_eq!(ack.error, None);
    }

    #[test]
    fn the_git_seed_round_trips() {
        let wire = r#"{"git":"https://github.com/example/repo"}"#;
        let seed: SeedRequest = serde_json::from_str(wire).unwrap();
        assert_eq!(
            seed,
            SeedRequest::Git {
                git: "https://github.com/example/repo".into(),
            },
        );
        assert_eq!(
            serde_json::to_value(&seed).unwrap(),
            serde_json::from_str::<serde_json::Value>(wire).unwrap(),
        );
    }

    #[test]
    fn the_tarball_seed_round_trips() {
        let wire = r#"{"tarball_base64":"aGk=","tarball_name":"repo.tar.gz"}"#;
        let seed: SeedRequest = serde_json::from_str(wire).unwrap();
        assert_eq!(
            seed,
            SeedRequest::Tarball {
                tarball_base64: "aGk=".into(),
                tarball_name: Some("repo.tar.gz".into()),
            },
        );
        assert_eq!(
            serde_json::to_value(&seed).unwrap(),
            serde_json::from_str::<serde_json::Value>(wire).unwrap(),
        );
    }

    #[test]
    fn a_tarball_seed_without_a_name_omits_the_key() {
        let seed: SeedRequest = serde_json::from_str(r#"{"tarball_base64":"aGk="}"#).unwrap();
        assert_eq!(
            seed,
            SeedRequest::Tarball {
                tarball_base64: "aGk=".into(),
                tarball_name: None,
            },
        );
        assert_eq!(
            serde_json::to_value(&seed).unwrap(),
            serde_json::json!({ "tarball_base64": "aGk=" }),
        );
    }

    #[test]
    fn neither_seed_kind_is_a_shape() {
        // The shim clones nothing and unpacks nothing for an empty
        // body; the protocol refuses it rather than shipping a seed
        // with no contents.
        assert!(serde_json::from_str::<SeedRequest>("{}").is_err());
    }

    #[test]
    fn stdout_streams_before_stderr() {
        let reply = ExecReply {
            exit_code: Some(0),
            stdout: "one\ntwo\n".into(),
            stderr: "warn\nboom\n".into(),
            timed_out: false,
        };
        let mut lines: Vec<String> = Vec::new();
        {
            let mut progress = |line: &str| lines.push(line.to_owned());
            stream_reply(&reply, &mut progress);
        }
        assert_eq!(lines, vec!["one\n", "two\n", "warn\n", "boom\n"]);
    }

    #[test]
    fn an_unterminated_last_line_still_ends_with_a_newline() {
        let reply = ExecReply {
            exit_code: Some(0),
            stdout: "no trailing newline".into(),
            stderr: String::new(),
            timed_out: false,
        };
        let mut lines: Vec<String> = Vec::new();
        {
            let mut progress = |line: &str| lines.push(line.to_owned());
            stream_reply(&reply, &mut progress);
        }
        assert_eq!(lines, vec!["no trailing newline\n"]);
    }
}
