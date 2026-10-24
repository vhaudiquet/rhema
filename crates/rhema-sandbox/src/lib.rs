//! rhema-sandbox — the sandbox seam: the `Sandbox` contract, the
//! workspace `bash` runs in, and the `ShellSandbox` adapter.
//!
//! One seam any frontend can aim at any machine — the host, a
//! container, a VM, a box over SSH, a cloud sandbox — chosen by
//! configuration, never by which crate a frontend was born in. The
//! family mirrors the provider family: the contract and the shared
//! machinery in one crate, one adapter per backend beside it; the
//! backends (`local`, `client`, `server`) land feature-gated in their
//! own phases, and the scripted mock rides the `mock` feature so tests
//! run the same seam against a fake. In the cloud deployment the seam
//! is a Cloudflare Sandbox — a container bound to the session's
//! Durable Object, persisted across turns and idle sleeps.
//!
//! The trait is object-safe and async, with `Send` futures like every
//! seam in this tree, and carries no platform dependency — no
//! wasm-bindgen, no fetch, no `worker`: the platform lives in the
//! transport that wraps it, not here. [`ShellSandbox`] adapts the seam
//! onto [`ShellExecutor`](rhema_agent_tools::ShellExecutor), so the
//! `bash` tool the model sees is unchanged wherever commands run.
//!
//! During the transition `rhema-cloud-agent` re-exports these names so
//! its public surface does not churn; new consumers import them from
//! here.
#![forbid(unsafe_code)]
#![deny(missing_docs)]

pub mod adapter;
#[cfg(feature = "mock")]
pub mod mock;

pub use adapter::ShellSandbox;

use std::sync::atomic::AtomicBool;
use std::time::Duration;

use async_trait::async_trait;

/// One command to run in the sandbox workspace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecRequest {
    /// The command line, run with `bash -c`.
    pub command: String,
    /// Working directory inside the sandbox, POSIX-style (`/workspace`).
    /// A string, not a `PathBuf`: the path is meaningful only inside the
    /// sandbox's filesystem, and no host should be tempted to touch it.
    pub cwd: String,
    /// Wall-clock budget. The sandbox kills the command — and every
    /// process it started — past it, and reports
    /// [`SandboxError::Timeout`].
    pub timeout: Duration,
}

/// What a command left behind. The sandbox side tail-caps each stream
/// before it crosses the wire, so a runaway command cannot flood the
/// transport; the tool applies its display caps again host-side.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecOutcome {
    /// Everything the command wrote to stdout, tail-capped.
    pub stdout: Vec<u8>,
    /// Everything the command wrote to stderr, tail-capped.
    pub stderr: Vec<u8>,
    /// The exit code; `None` when the process died by signal.
    pub code: Option<i32>,
}

/// How a fresh workspace gets its contents. The sandbox persists across
/// turns and idle sleeps; a full container eviction loses the files, and
/// the next session re-seeds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkspaceSeed {
    /// `git clone <url> .` into the workspace root — the repository
    /// becomes the workspace.
    GitClone(String),
    /// A gzipped tarball, base64-encoded, unpacked at the workspace
    /// root.
    Tarball {
        /// A file name for the upload, diagnostics only.
        name: String,
        /// The tarball's bytes, base64.
        tar_gz_base64: String,
    },
}

/// Why a sandbox operation failed. In-band: the shell tool turns every
/// variant into an `is_error` result for the model, never an abort.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SandboxError {
    /// The sandbox could not be reached: the container is not up, the
    /// transport failed, or the response was malformed.
    #[error("sandbox unavailable: {0}")]
    Unavailable(String),
    /// The command ran past its budget and was killed.
    #[error("command timed out after {0}s and was killed")]
    Timeout(u64),
    /// The operation failed inside the sandbox: a missing file, a clone
    /// that would not clone.
    #[error("sandbox command failed: {0}")]
    Failed(String),
}

/// The sandbox a session's commands run in. One instance per session,
/// shared by the turn's shell, the skills seeding, and any subagents.
#[async_trait]
pub trait Sandbox: Send + Sync {
    /// Runs `request.command` with `bash -c` and `request.cwd` as
    /// working directory, streaming complete output lines through
    /// `progress` as they land (v1 transports may deliver the whole
    /// output at completion; the callback shape is the contract either
    /// way). A flip of `cancelled` stops the run where the transport
    /// can observe it — before the command starts, and between chunks.
    async fn exec(
        &self,
        request: ExecRequest,
        cancelled: &AtomicBool,
        progress: &mut (dyn for<'a> FnMut(&'a str) + Send),
    ) -> Result<ExecOutcome, SandboxError>;

    /// Seeds a fresh workspace. Calling this on a workspace that already
    /// has contents replaces them; callers seed when the DO storage says
    /// the workspace is new or was evicted, not on every turn.
    async fn seed(&self, seed: &WorkspaceSeed) -> Result<(), SandboxError>;

    /// Reads a file from the workspace.
    async fn read_file(&self, path: &str) -> Result<Vec<u8>, SandboxError>;

    /// Writes (or replaces) a file in the workspace. This is how the
    /// session's skills and SYSTEM.md reach the sandbox — the model
    /// reads them through `bash`, like any other file.
    async fn write_file(&self, path: &str, contents: &[u8]) -> Result<(), SandboxError>;
}
