//! rhema-sandbox, the sandbox seam: the `Sandbox` contract, the
//! workspace `bash` runs in, and the `ShellSandbox` adapter.
//!
//! One seam any frontend can aim at any machine, the host, a
//! container, a VM, a box over SSH, a cloud sandbox, chosen by
//! configuration, never by which crate a frontend was born in. The
//! family mirrors the provider family: this crate is its base, the
//! contract ([`Sandbox`]), the [`ShellSandbox`] adapter, the wire
//! protocol ([`protocol`]), the seeding helper ([`seed_workspace`]),
//! the scripted mock ([`mock`], the `mock` feature), the instances'
//! cross-process lock ([`shared`]), and, behind the `spawn` feature,
//! the process-spawning family's shared machinery
//! ([`exec_util`], [`fsroot`]). The backends are sibling crates: the
//! process family, `rhema-sandbox-local`, the commands as host
//! processes rooted at one host directory; `rhema-sandbox-docker`, the
//! bind-mounted container driven through the `docker` CLI;
//! `rhema-sandbox-ssh`, the daemonless remote over `ssh`, plus
//! `rhema-sandbox-http`, the protocol's HTTPS client, and
//! `rhema-sandbox-server`, the `rhema-sandboxd` daemon. In the cloud
//! deployment the seam is a Cloudflare Sandbox, a container bound to
//! the session's Durable Object, persisted across turns and idle
//! sleeps.
//!
//! The trait is object-safe and async, with `Send` futures like every
//! seam in this tree, and carries no platform dependency, no
//! wasm-bindgen, no fetch, no `worker`: the platform lives in the
//! transport that wraps it, not here. [`ShellSandbox`] adapts the seam
//! onto [`ShellExecutor`](rhema_bash::ShellExecutor), so the
//! `bash` tool the model sees is unchanged wherever commands run.
//!
//! During the transition `rhema-cloud-agent` re-exports these names so
//! its public surface does not churn; new consumers import them from
//! here.
#![forbid(unsafe_code)]

pub mod adapter;
// The whole seconds a budget names, ceiled, never floored, so a
// sub-second budget cannot render as `0s` — or reach a far-side
// `timeout 0` prefix, which is no timeout at all — on the wire or
// in a report. Ungated at the root because the protocol needs it
// everywhere: a wasm client serializes the same ceiling a unix
// host's spawned prefixes and reports use, one helper for the
// whole fleet (the bash tool's own executor defines it).
pub use rhema_bash::exec::budget_secs;
#[cfg(feature = "spawn")]
pub mod exec_util;
#[cfg(feature = "spawn")]
pub mod fsroot;
pub mod kit;
#[cfg(feature = "mock")]
pub mod mock;
pub mod protocol;
pub mod seed;
pub mod shared;

pub use adapter::ShellSandbox;
#[cfg(feature = "spawn")]
pub use fsroot::FsRoot;
pub use protocol::{
    Ack, EXEC, ExecCall, ExecReply, FileReply, GET, PUT, PutFile, SEED, SeedRequest, TOKEN_HEADER,
    stream_reply,
};
pub use rhema_bash::exec::{RunOutcome, RunRequest};
pub use rhema_core::{OutputOnly, ToolProgress};
pub use seed::seed_workspace;
pub use shared::InstanceLock;

use std::sync::atomic::AtomicBool;

use async_trait::async_trait;

/// How a fresh workspace gets its contents. The sandbox persists across
/// turns and idle sleeps; a full container eviction loses the files, and
/// the next session re-seeds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkspaceSeed {
    /// `git clone <url> .` into the workspace root, the repository
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
#[non_exhaustive]
pub enum SandboxError {
    /// The sandbox could not be reached: the container is not up, the
    /// transport failed, or the response was malformed.
    #[error("sandbox unavailable: {0}")]
    Unavailable(String),
    /// The command ran past its budget and was killed.
    #[error("command timed out after {0}s and was killed")]
    Timeout(u64),
    /// The command was killed by a SIGKILL from outside its time
    /// budget, most often the machine's out-of-memory killer; the
    /// budget's own kills read as [`SandboxError::Timeout`].
    #[error(
        "the command was killed (SIGKILL, exit 137): most often the machine's out-of-memory killer, not the time budget"
    )]
    OutOfMemory,
    /// The operation failed inside the sandbox: a missing file, a clone
    /// that would not clone.
    #[error("sandbox command failed: {0}")]
    Failed(String),
}

impl From<std::io::Error> for SandboxError {
    fn from(err: std::io::Error) -> Self {
        Self::Unavailable(err.to_string())
    }
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
    /// can observe it, before the command starts, and between chunks.
    async fn run(
        &self,
        request: RunRequest,
        cancelled: &AtomicBool,
        progress: &mut dyn ToolProgress,
    ) -> Result<RunOutcome, SandboxError>;

    /// Seeds a fresh workspace. Calling this on a workspace that already
    /// has contents replaces them; callers seed when the DO storage says
    /// the workspace is new or was evicted, not on every turn.
    async fn seed(&self, seed: &WorkspaceSeed) -> Result<(), SandboxError>;

    /// Reads a file from the workspace.
    async fn read_file(&self, path: &str) -> Result<Vec<u8>, SandboxError>;

    /// Writes (or replaces) a file in the workspace. This is how the
    /// session's skills and SYSTEM.md reach the sandbox, the model
    /// reads them through `bash`, like any other file.
    async fn write_file(&self, path: &str, contents: &[u8]) -> Result<(), SandboxError>;
}
