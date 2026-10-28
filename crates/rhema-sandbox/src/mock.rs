//! The scripted sandbox for offline tests: the same [`Sandbox`] seam the
//! DO drives, answering from a script, recording what it was asked. The
//! house pattern is `rhema-core`'s scripted `MockProvider`; this is its
//! sandbox twin, and the two together run the full turn loop on any
//! host with no network and no container.
#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex, PoisonError};

use async_trait::async_trait;

use crate::{ExecOutcome, ExecRequest, Sandbox, SandboxError, WorkspaceSeed};

/// What the next `exec` answers, queued by the test.
type ScriptedOutcome = Result<ExecOutcome, SandboxError>;

/// A sandbox that answers from a script.
#[derive(Default)]
pub struct ScriptedSandbox {
    /// Queued answers, popped in order; an empty queue answers success
    /// with empty output, so tests script only what they assert on.
    outcomes: Mutex<Vec<ScriptedOutcome>>,
    /// Every command handed to `exec`, in order.
    executed: Mutex<Vec<String>>,
    /// When set, the next `exec` parks until [`Self::release`] — the
    /// deterministic "crash mid-command" point: a turn task dropped
    /// while parked dies between commit and result. The channel's two
    /// halves travel together so the parked side has a live sender to
    /// wait on; `release` drops the pair, which unblocks the exec.
    hold: Mutex<
        Option<(
            tokio::sync::oneshot::Sender<()>,
            tokio::sync::oneshot::Receiver<()>,
        )>,
    >,
    /// The workspace files `write_file` put here and `read_file` reads
    /// back.
    files: Mutex<BTreeMap<String, Vec<u8>>>,
    /// Every seed the sandbox was asked to apply.
    seeds: Mutex<Vec<WorkspaceSeed>>,
    /// Fires when a held `exec` starts parking — the deterministic
    /// point a test waits for before aborting the turn.
    parked: Arc<tokio::sync::Notify>,
}

impl ScriptedSandbox {
    /// An empty sandbox: nothing scripted, nothing executed.
    pub fn new() -> Self {
        Self::default()
    }

    /// Queues the next `exec` answer.
    pub fn push_exec(&self, outcome: ScriptedOutcome) {
        self.outcomes
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(outcome);
    }

    /// The signal a held `exec` sends when it starts parking. Register
    /// a `notified()` future before starting the turn, so the wakeup
    /// cannot be missed.
    pub fn parked(&self) -> &tokio::sync::Notify {
        &self.parked
    }

    /// Makes the next `exec` park until [`Self::release`] — or forever,
    /// which is how a turn is made to die mid-command.
    pub fn hold_next_exec(&self) {
        // The sender stays with the receiver: a sender dropped here
        // would close the channel at once and the "park" would never
        // hold.
        let channel = tokio::sync::oneshot::channel::<()>();
        *self.hold.lock().unwrap_or_else(PoisonError::into_inner) = Some(channel);
    }

    /// Releases a parked `exec`, if one is parked.
    pub fn release(&self) {
        self.hold
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
    }

    /// The commands executed so far, in order.
    pub fn executed(&self) -> Vec<String> {
        self.executed
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// The files currently in the workspace.
    pub fn files(&self) -> BTreeMap<String, Vec<u8>> {
        self.files
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// The seeds applied so far, in order.
    pub fn seeds(&self) -> Vec<WorkspaceSeed> {
        self.seeds
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

#[async_trait]
impl Sandbox for ScriptedSandbox {
    async fn exec(
        &self,
        request: ExecRequest,
        _cancelled: &AtomicBool,
        _progress: &mut (dyn for<'a> FnMut(&'a str) + Send),
    ) -> Result<ExecOutcome, SandboxError> {
        self.executed
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(request.command);
        // The parked call: a turn aborted here never returns.
        let held = self
            .hold
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some((sender, receiver)) = held {
            self.parked.notify_one();
            // The sender lives through the await: dropping it early
            // would close the channel and end the park at once.
            let _park_open = sender;
            let _ = receiver.await;
        }
        // An empty script answers success: a test scripts only what it
        // asserts on.
        let mut outcomes = self.outcomes.lock().unwrap_or_else(PoisonError::into_inner);
        let outcome = if outcomes.is_empty() {
            Ok(ExecOutcome {
                stdout: Vec::new(),
                stderr: Vec::new(),
                code: Some(0),
            })
        } else {
            outcomes.remove(0)
        };
        drop(outcomes);
        outcome
    }

    async fn seed(&self, seed: &WorkspaceSeed) -> Result<(), SandboxError> {
        self.seeds
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(seed.clone());
        Ok(())
    }

    async fn read_file(&self, path: &str) -> Result<Vec<u8>, SandboxError> {
        self.files
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(path)
            .cloned()
            .ok_or_else(|| SandboxError::Failed(format!("no such file: {path}")))
    }

    async fn write_file(&self, path: &str, contents: &[u8]) -> Result<(), SandboxError> {
        self.files
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(path.to_owned(), contents.to_vec());
        Ok(())
    }
}
