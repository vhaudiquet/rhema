//! The `bash` tool over a sandbox.
//!
//! [`ShellSandbox`] adapts the [`Sandbox`] seam onto
//! [`ShellExecutor`](rhema_agent_tools::ShellExecutor), so the tool the
//! model sees is exactly the built-in one —
//! [`ShellTools::builtin_specs`] is the single source of truth for the
//! spec, and the approval gate, the output caps, and the error shaping
//! all stay in [`ShellTools`]. Nothing about the spec text says where
//! the command runs, and nothing here forks it.
#![forbid(unsafe_code)]

use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use async_trait::async_trait;
use rhema_agent_tools::exec::{Executed, ShellError, ShellExecutor};

use crate::{ExecRequest, Sandbox, SandboxError};

/// The shell executor that runs every command in a [`Sandbox`].
pub struct ShellSandbox {
    sandbox: Arc<dyn Sandbox>,
}

impl ShellSandbox {
    /// An executor over `sandbox`.
    pub fn new(sandbox: Arc<dyn Sandbox>) -> Self {
        Self { sandbox }
    }
}

/// Maps a sandbox failure onto the shell error the tool shapes into an
/// `is_error` result.
fn shell_error(err: SandboxError) -> ShellError {
    match err {
        SandboxError::Timeout(secs) => ShellError::Timeout(secs),
        SandboxError::Unavailable(msg) | SandboxError::Failed(msg) => ShellError::Failed(msg),
    }
}

#[async_trait]
impl ShellExecutor for ShellSandbox {
    async fn run(
        &self,
        command: &str,
        cwd: &std::path::Path,
        timeout: Duration,
        cancelled: &AtomicBool,
        progress: &mut (dyn for<'a> FnMut(&'a str) + Send),
    ) -> Result<Executed, ShellError> {
        // The workspace path is a POSIX path inside the sandbox; it rode
        // through a `PathBuf` unchanged and leaves as text.
        let request = ExecRequest {
            command: command.to_owned(),
            cwd: cwd.to_string_lossy().into_owned(),
            timeout,
        };
        let outcome = self
            .sandbox
            .exec(request, cancelled, progress)
            .await
            .map_err(shell_error)?;
        Ok(Executed {
            stdout: outcome.stdout,
            stderr: outcome.stderr,
            code: outcome.code,
        })
    }
}
