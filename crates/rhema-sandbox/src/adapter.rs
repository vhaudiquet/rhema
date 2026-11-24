//! The `bash` tool over a sandbox.
//!
//! [`ShellSandbox`] adapts the [`Sandbox`] seam onto
//! [`ShellExecutor`], so the tool the
//! model sees is exactly the built-in one,
//! `ShellTools::builtin_specs` is the single source of truth for the
//! spec, and the approval gate, the output caps, and the error shaping
//! all stay in `ShellTools`. Nothing about the spec text says where
//! the command runs, and nothing here forks it.
#![forbid(unsafe_code)]

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use async_trait::async_trait;
use rhema_bash::exec::{ShellError, ShellExecutor};

use crate::{RunOutcome, RunRequest, Sandbox, SandboxError, ToolProgress};

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
/// `is_error` result. The family's cancel crosses the seam as the
/// `cancelled` sentinel text, and reads here as the executor's own
/// [`ShellError::Cancelled`], so the model sees one wording wherever
/// the command ran, not "sandbox command failed: cancelled".
fn shell_error(err: SandboxError) -> ShellError {
    match err {
        SandboxError::Timeout(secs) => ShellError::Timeout(secs),
        SandboxError::OutOfMemory => ShellError::OutOfMemory,
        SandboxError::Unavailable(msg) | SandboxError::Failed(msg) => {
            if msg == "cancelled" {
                ShellError::Cancelled
            } else {
                ShellError::Failed(msg)
            }
        }
    }
}

#[async_trait]
impl ShellExecutor for ShellSandbox {
    async fn run(
        &self,
        request: RunRequest,
        cancelled: &AtomicBool,
        progress: &mut dyn ToolProgress,
    ) -> Result<RunOutcome, ShellError> {
        self.sandbox
            .run(request, cancelled, progress)
            .await
            .map_err(shell_error)
    }
}

#[cfg(all(test, feature = "mock"))]
mod tests {
    use super::*;
    use crate::mock::ScriptedSandbox;
    use crate::{OutputOnly, RunRequest};
    use std::time::Duration;

    /// One scripted run through the adapter, the error examined.
    async fn mapped(error: SandboxError) -> ShellError {
        let sandbox = ScriptedSandbox::new();
        sandbox.push_run(Err(error));
        let executor = ShellSandbox::new(std::sync::Arc::new(sandbox));
        let cancelled = AtomicBool::new(false);
        let mut sink = OutputOnly(|_| {});
        executor
            .run(
                RunRequest {
                    command: "x".to_owned(),
                    cwd: "/".into(),
                    timeout: Duration::from_secs(1),
                },
                &cancelled,
                &mut sink,
            )
            .await
            .expect_err("the script answers an error")
    }

    // The family's cancel sentinel reads as the shell's own
    // cancellation, the same wording the local executor gives a flip.
    #[tokio::test]
    async fn the_cancel_sentinel_reads_as_cancelled() {
        assert_eq!(
            mapped(SandboxError::Failed("cancelled".into())).await,
            ShellError::Cancelled
        );
    }

    #[tokio::test]
    async fn any_other_failure_keeps_its_own_text() {
        assert_eq!(
            mapped(SandboxError::Failed("no such file".into())).await,
            ShellError::Failed("no such file".into())
        );
    }
}
