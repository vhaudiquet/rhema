//! The ecrou backend: one microVM per command, the workspace as a
//! share.
//!
//! [`EcrouSandbox`] is the fs-sharing family's microVM member (spec
//! §4's "VM / microVM" row, the ecrou shape): the host workspace
//! crosses every boot as a virtiofs share, so the file routes are
//! plain fs through [`FsRoot`], as on the docker bind mount, and
//! only exec crosses the `ecrou` CLI. There is no instance to stand
//! up or stop: ecrou runs one-shot machines, so every exec boots a
//! fresh VM from the image, runs the command with the workspace
//! shared in, and powers the machine off when the command exits.
//! By default rootfs state, package installs, daemons, `/tmp`,
//! lives exactly one command and the workspace is what persists.
//! With [`EcrouSandbox::with_persist`] the session names a persisted
//! state (`--persist KEY`): the boots share one writable upper over a
//! read-only base, so an install is the next command's starting
//! point, and a command that fails or dies drops its changes. The
//! workspace persists either way, which is the seam's whole contract.
//!
//! Exec is `ecrou run --quiet --capture <io> -v <ws>:<ws>
//! [--persist KEY] --workdir <cwd> <image> -- /bin/sh -c <line>`: the
//! workspace share
//! mounts at its own host path (the docker 1:1 posture), the cwd
//! rides `--workdir`, and the line runs the command through `bash -c`
//! when the image carries bash, through its `sh` when it does not.
//! The capture directory is a scratch tempdir ecrou takes as an extra
//! share: the payload's stdout and stderr land there as two files,
//! the console (kernel and boot chatter, merged) is discarded, and
//! the pump tails the two files while the machine runs, complete
//! lines through `progress`, a capped tail each, the same shape
//! `exec_util` gives the pipe-backed backends. The scratch roots
//! where the `ecrou` on PATH can see it: `/tmp` for a native build,
//! the snap's durable area for the snap, whose private `/tmp` would
//! take the files and swallow the output. The same boundary rules
//! the machine's git: the host's effective config is flattened into
//! `.rhema/git/config` inside the workspace and exec points
//! `GIT_CONFIG_GLOBAL` at it, so commits inside carry the host's
//! identity without a config mount ever crossing.
//!
//! The budget and the cancel are the kill, host-side: killing the
//! `ecrou` process kills the machine with it (the VMM child carries
//! `PR_SET_PDEATHSIG`), stronger than the docker prefix, which leaves
//! the command running in the container. The boot rides inside the
//! wall-clock budget, a cold image pays its download once into the
//! ecrou cache, every command pays its extract-plus-boot (under 2 s
//! on a warm alpine, more on a distro rootfs). Anything `ecrou`
//! itself reports (`Error:` on its stderr) is an unavailable sandbox,
//! not a guest result. One refusal is answered instead of reported:
//! ecrou refuses to boot a persisted state against a base it was not
//! built on (`persist base mismatch`), and the retry resets the state
//! and says so through `progress`, the state is regenerable, the
//! model should know its installs are gone.
#![forbid(unsafe_code)]

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use base64::Engine;
use rhema_sandbox::exec_util::{self, STAGING, under_workspace};
use rhema_sandbox::fsroot::FsRoot;
use rhema_sandbox::{RunOutcome, RunRequest, Sandbox, SandboxError, ToolProgress, WorkspaceSeed};

mod cli;

pub use cli::shared_root;
mod gitenv;
mod tail;

use cli::{capture_dir, run_argv};
use gitenv::GitEnv;
use tail::{StreamTail, classify};

/// The image a session boots when the profile names none:
/// `ghcr.io/vhaudiquet/rhema-images:latest`, the rhema-ready base. The
/// tools the shell session expects (`git`, `rg`, `fd`, `jq`, `yq`,
/// `sd`, `curl`, `python3`) are baked in, because a one-shot machine
/// cannot be provisioned after boot: the machine's tools are the
/// image's business. `latest` floats with the base image's releases
/// and refreshes through `ecrou pull`; a workspace that needs
/// reproducibility pins the digest in the profile.
pub const DEFAULT_IMAGE: &str = "ghcr.io/vhaudiquet/rhema-images:latest";

/// The seed budget for both seed kinds, the siblings' number: the
/// seed runs host-side, so the budget is the same `timeout` prefix
/// every host command here gets.
const SEED_BUDGET: Duration = Duration::from_secs(300);

/// How often the capture files are tailed and the cancel flag and the
/// budget sampled. The machine has no stdout to block on, the
/// console rides a nulled pipe, so this tick is the only observation
/// point a silent command's kill has.
const TICK: Duration = Duration::from_millis(100);

/// The ecrou sandbox a session's commands run in: every exec boots
/// one microVM of `image` with the host root shared in at its own
/// path.
#[derive(Debug, Clone)]
pub struct EcrouSandbox {
    /// The image reference every boot resolves: an ecrou distro name
    /// (`ubuntu`, `alpine:3.23`) or an OCI image reference.
    pub(crate) image: String,
    /// The host directory standing in as the sandbox filesystem, the
    /// host side of the workspace share.
    pub(crate) root: FsRoot,
    /// The sandbox-side path of the workspace: the path the prompt
    /// names, stripped from sandbox paths before they map through
    /// [`FsRoot`] or join the guest's mount point.
    pub(crate) workspace: String,
    /// The persist key, when the session keeps rootfs state between
    /// commands: `--persist KEY` rides every exec, the base stays
    /// read-only, and what the commands install survives them in the
    /// key's upper. `None` is the one-command rootfs.
    pub(crate) persist: Option<String>,
    /// The backend binary every boot spawns: `ecrou` in production, a
    /// fake the tests substitute.
    pub(crate) bin: String,
    /// The runtime root shared in at its own path on every boot, when
    /// the session has one: the kit install and the per-session tree.
    pub(crate) shared: Option<String>,
}

impl EcrouSandbox {
    /// A sandbox booting `image` with the host root `root` shared in,
    /// named `workspace` on the sandbox side.
    pub fn new(image: impl Into<String>, root: FsRoot, workspace: impl Into<String>) -> Self {
        Self {
            image: image.into(),
            root,
            workspace: workspace.into(),
            persist: None,
            bin: "ecrou".to_owned(),
            shared: None,
        }
    }

    /// Shares the host runtime root at its own path on every boot. A
    /// one-shot guest sees the install and the session tree through
    /// the workspace-share mechanism.
    pub fn with_shared_root(mut self, path: impl Into<String>) -> Self {
        self.shared = Some(path.into());
        self
    }

    /// Names the persisted state rootfs changes land in. The key is
    /// the workspace's derivation, stable across sessions, so a
    /// session's installs are its next command's starting point.
    pub fn with_persist(mut self, key: impl Into<String>) -> Self {
        self.persist = Some(key.into());
        self
    }

    /// The guest path of the sandbox cwd: the workspace share mounts
    /// at its own canonical host path, so the mount point plus the
    /// prefix-stripped tail is where the command lands. The empty and
    /// root cwd land at the mount itself.
    fn guest_cwd(&self, cwd: &str) -> Result<String, SandboxError> {
        // The guard first: the cwd is model-influenced, and the root
        // refuses relatives, climbs, and link escapes.
        self.root.map(cwd)?;
        let tail = under_workspace(&self.workspace, cwd);
        let mount = self.mount_point();
        Ok(match tail.as_str() {
            "" | "/" => mount,
            _ => format!("{mount}{tail}"),
        })
    }

    /// The workspace share's host path, canonical, the path both
    /// sides of the share use, so a workspace reached through a link
    /// still mounts under the name its files resolve to.
    fn mount_point(&self) -> String {
        self.root
            .map("")
            .map(|path| path.to_string_lossy().into_owned())
            .unwrap_or_default()
    }
}

#[async_trait]
impl Sandbox for EcrouSandbox {
    async fn run(
        &self,
        request: RunRequest,
        cancelled: &AtomicBool,
        progress: &mut dyn ToolProgress,
    ) -> Result<RunOutcome, SandboxError> {
        // The trait's first observation point: a flip before the start
        // never boots a machine.
        if cancelled.load(Ordering::Relaxed) {
            return Err(SandboxError::Failed("cancelled".into()));
        }
        let workdir = self.guest_cwd(&request.cwd.to_string_lossy())?;
        let mount = self.mount_point();
        // The capture directory lives one call: the guest writes the
        // payload's streams into it through the share ecrou takes,
        // and the drop deletes whatever the run left.
        let capture = capture_dir()?;
        // The machine's git reads the host's identity: the staged
        // config crosses the workspace share, and this env pair points
        // every git invocation in the payload at it. Absent (no host
        // config, no git here) the pair is simply not passed, and git
        // behaves as it always has.
        let git_env = self.stage_git_config();
        // One deadline for the whole request, the mismatch retry
        // included: the boot that discovered the mismatch already
        // spent part of the budget, and the reset boot runs on what
        // is left, so one nominal budget cannot pay for two boots.
        let deadline = Instant::now() + request.timeout;
        let outcome = self
            .run_machine(
                &workdir,
                &mount,
                &request,
                cancelled,
                progress,
                &capture,
                git_env.as_ref(),
                false,
                deadline,
            )
            .await;
        match outcome {
            // The persisted state is the workspace's, and ecrou
            // refuses to boot it against a base it was not built on.
            // The state is regenerable and the refusal exists to be
            // answered: reset and retry once, saying plainly what the
            // reset costs, the session's installs are gone.
            Err(SandboxError::Unavailable(detail))
                if detail.contains("persist base mismatch") && self.persist.is_some() =>
            {
                progress.output(
                    "# ecrou: the sandbox image changed since this workspace's state was saved; \
                     the state was reset and this command starts fresh (reinstall anything missing)\n",
                );
                self.run_machine(
                    &workdir,
                    &mount,
                    &request,
                    cancelled,
                    progress,
                    &capture,
                    git_env.as_ref(),
                    true,
                    deadline,
                )
                .await
            }
            other => other,
        }
    }

    async fn seed(&self, seed: &WorkspaceSeed) -> Result<(), SandboxError> {
        // The workspace is host fs: the seed needs no machine.
        match seed {
            WorkspaceSeed::GitClone(url) => self.run_host_seed(&["git", "clone", url, "."]).await,
            WorkspaceSeed::Tarball { tar_gz_base64, .. } => {
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(tar_gz_base64)
                    .map_err(|err| {
                        SandboxError::Failed(format!("the seed tarball is not base64: {err}"))
                    })?;
                self.write_file(&format!("/{STAGING}"), &bytes).await?;
                let unpacked = self.run_host_seed(&["tar", "-xzf", STAGING]).await;
                // The staging file is not workspace content; a leftover
                // one is harmless, so its removal is best-effort.
                if let Ok(staged) = self.root.map(&format!("/{STAGING}")) {
                    let _ = tokio::fs::remove_file(staged).await;
                }
                unpacked
            }
        }
    }

    async fn read_file(&self, path: &str) -> Result<Vec<u8>, SandboxError> {
        exec_util::read_through(&self.root, &under_workspace(&self.workspace, path)).await
    }

    async fn write_file(&self, path: &str, contents: &[u8]) -> Result<(), SandboxError> {
        exec_util::write_through(
            &self.root,
            &under_workspace(&self.workspace, path),
            contents,
        )
        .await
    }
}

impl EcrouSandbox {
    /// Boots one machine for `request` and sees it through: spawn,
    /// the capture-file pump, the budget and cancel, the classify.
    /// One call is one `ecrou run`; the reset retry calls this twice
    /// under the caller's `deadline`, the same request-budget wall
    /// clock, so the retry runs on the budget's remainder.
    #[allow(clippy::too_many_arguments)]
    async fn run_machine(
        &self,
        workdir: &str,
        mount: &str,
        request: &RunRequest,
        cancelled: &AtomicBool,
        progress: &mut dyn ToolProgress,
        capture: &tempfile::TempDir,
        git_env: Option<&GitEnv>,
        persist_reset: bool,
        deadline: Instant,
    ) -> Result<RunOutcome, SandboxError> {
        use std::process::Stdio;

        let mount_share = format!("{mount}:{mount}");
        let mut shares = vec![mount_share];
        if let Some(shared) = &self.shared {
            shares.push(format!("{shared}:{shared}"));
        }
        let shares: Vec<&str> = shares.iter().map(String::as_str).collect();
        let argv = run_argv(
            &self.bin,
            &self.image,
            capture.path(),
            &shares,
            workdir,
            &request.command,
            &request.env,
            git_env,
            self.persist.as_deref(),
            persist_reset,
        );
        let mut child = tokio::process::Command::new(&argv[0])
            .args(&argv[1..])
            // The console (merged kernel and boot chatter) is this
            // backend's discarded stream; stderr carries only ecrou's
            // own words, drained after the exit.
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            // The explicit kill below is the budget and the cancel;
            // the drop is the backstop for the early returns, and a
            // killed ecrou takes the machine down with it.
            .kill_on_drop(true)
            .spawn()
            .map_err(|err| SandboxError::Unavailable(format!("{err}")))?;

        let mut out = StreamTail {
            path: capture.path().join("stdout"),
            ..StreamTail::default()
        };
        let mut err = StreamTail {
            path: capture.path().join("stderr"),
            ..StreamTail::default()
        };
        let code = loop {
            tokio::select! {
                _ = tokio::time::sleep(TICK) => {
                    out.pump(progress, false).await?;
                    err.pump(progress, false).await?;
                    if cancelled.load(Ordering::Relaxed) {
                        let _ = child.start_kill();
                        let _ = child.wait().await;
                        return Err(SandboxError::Failed("cancelled".into()));
                    }
                    if Instant::now() >= deadline {
                        let _ = child.start_kill();
                        let _ = child.wait().await;
                        return Err(SandboxError::Timeout(rhema_sandbox::exec_util::budget_secs(
                        request.timeout,
                    )));
                    }
                }
                status = child.wait() => {
                    break status.map_err(|err| {
                        SandboxError::Unavailable(format!("waiting for the machine: {err}"))
                    })?;
                }
            }
        };
        // The machine is gone; the last output landed in the capture
        // files right before it died. Drain once more, partial line
        // included.
        out.pump(progress, true).await?;
        err.pump(progress, true).await?;
        // Nothing reads stderr while the machine runs, ecrou writes
        // a warning or two, never a stream, so the drain after the
        // exit cannot have filled the pipe.
        let mut ecrou_stderr = Vec::new();
        if let Some(mut pipe) = child.stderr.take() {
            tokio::io::AsyncReadExt::read_to_end(&mut pipe, &mut ecrou_stderr)
                .await
                .map_err(|err| SandboxError::Unavailable(format!("{err}")))?;
        }
        classify(
            code.code(),
            String::from_utf8_lossy(&ecrou_stderr).into_owned(),
            out.take(),
            err.take(),
        )
    }
    /// Runs one seed command on the host, at the root the machines
    /// share: the budget and kill are the same `timeout` prefix, and
    /// a nonzero exit surfaces the stderr tail, the way the shim's
    /// seed route reports a failed clone or unpack.
    async fn run_host_seed(&self, argv: &[&str]) -> Result<(), SandboxError> {
        let root = self.root.map("/")?;
        let output = tokio::process::Command::new("timeout")
            .arg("--kill-after=5")
            .arg(SEED_BUDGET.as_secs().to_string())
            .args(argv)
            .current_dir(root)
            .output()
            .await
            .map_err(|err| SandboxError::Unavailable(format!("{err}")))?;
        // The prefix's own codes decide: budget expiry, or the
        // external kill that must not read as one.
        rhema_sandbox::exec_util::classify_exit(output.status.code(), SEED_BUDGET.as_secs())?;
        if !output.status.success() {
            let detail = String::from_utf8_lossy(&output.stderr);
            let detail = detail.trim();
            return Err(SandboxError::Failed(if detail.is_empty() {
                "seed command failed".into()
            } else {
                detail.to_owned()
            }));
        }
        Ok(())
    }
}

#[cfg(all(test, unix))]
mod tests;
