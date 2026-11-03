//! Where `bash` runs: the sandbox choice's resolution and the assembly
//! that turns a non-local choice into the agent's tool set.
//!
//! The choice is [`SandboxChoice`](crate::args::SandboxChoice), stored
//! per profile and overridden by `RHEMA_SANDBOX`
//! (`specs/rhema-sandbox.md` §5). `local` is the zero-flag fast path:
//! it constructs nothing here, and the agent builds
//! `ShellTools::new(workspace)` exactly as it always has. Every other
//! kind builds an `Arc<dyn Sandbox>`, seeds it once per workspace, and
//! hands the agent a [`ShellSandbox`] executor instead — the tool's
//! spec, caps, and error shaping are unchanged wherever commands run.
//! Everything else stays host-side: profiles, sessions, config reads,
//! subagent definition discovery, the skills' discovery itself.
//!
//! A non-local choice is also a shared instance: the assembly runs
//! under the instance's cross-process lock ([`InstanceLock`]) —
//! exclusive for the launch, the devices, the provision, and the seed,
//! shared for the session — and the last session out stops a
//! rhema-derived instance, never one a stored target named.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context;
use rhema_agent_tools::{ShellTools, Skill};
use rhema_coding_agent::CodingAgentConfig;
use rhema_config::Profile;
use rhema_config::secrets::SecretStore;
use rhema_sandbox::{FsRoot, InstanceLock, Sandbox, ShellSandbox, seed_workspace};
use rhema_sandbox_docker::DockerSandbox;
use rhema_sandbox_ecrou::{DEFAULT_IMAGE, EcrouSandbox};
use rhema_sandbox_http::HttpSandbox;
use rhema_sandbox_lxd::LxdSandbox;
use rhema_sandbox_ssh::SshSandbox;

use crate::args::SandboxChoice;

mod machines;
mod provision;
mod seed;
#[cfg(test)]
mod tests;

pub use machines::{SandboxAvailability, SandboxMachines};
use provision::{ensure_container, ensure_lxd_instance, ensure_lxd_mount, workspace_owner};
use seed::{agents_skills_dir, discovered_skills, ensure_seeded, global_skills_dir};

/// The choice this profile runs `bash` under: `RHEMA_SANDBOX` first —
/// [`Profile::sandbox_kind`] already layers the env over the stored
/// kind — then the stored kind. Anything unset or unparseable reads as
/// [`SandboxChoice::Local`]: the zero-flag path must never break
/// because of a typo'd config.
pub fn resolve_choice(profile: &Profile) -> SandboxChoice {
    choice_from_kind(profile.sandbox_kind().as_deref())
}

/// [`resolve_choice`] over the already-env-layered kind — the testable
/// form. The env tier is injected the way `rhema-config`'s env-wins
/// tests inject it: as the value [`Profile::sandbox_kind`] would have
/// returned.
fn choice_from_kind(kind: Option<&str>) -> SandboxChoice {
    SandboxChoice::from_stored(kind).unwrap_or(SandboxChoice::Local)
}

/// What a non-local choice adds to the agent's config: the tool set,
/// the sandbox-side workspace path the prompt and the tools name, and
/// the skill list rendered into the prompt with paths inside that
/// workspace.
pub struct Sandboxed {
    /// The choice this assembly stands for: the session's active kind
    /// once the result lands on the app.
    pub choice: SandboxChoice,
    /// What the choice resolved to and bound to: the container, the LXD
    /// instance, the ssh host, the http URL, or the ecrou image. A
    /// profile switch compares this against its own resolution to keep
    /// a session that would land in the very same place from paying for
    /// a re-assembly.
    pub target: String,
    /// The shell whose commands run in the sandbox; the subagents'
    /// inner shell inherits its executor.
    pub tools: Arc<ShellTools>,
    /// The workspace path as the sandbox side sees it: `/workspace`
    /// over http, the workspace's own path for the mount and ssh
    /// postures.
    pub workspace: String,
    /// The discovered skills; every `Skill::path` resolves inside the
    /// sandbox's filesystem — [`Self::workspace`] itself for the seeded
    /// postures, the workspace plus the two read-only skills mounts
    /// (rhema's own and the cross-agent `.agents` tree) for the mounted
    /// ones — because the model reads the bodies through the sandbox's
    /// shell.
    pub skills: Vec<Skill>,
    /// The shared instance's lock, held shared for the session: the
    /// mounted choices (docker, lxd) only. Dropping it is the release.
    pub(crate) instance_lock: Option<InstanceLock>,
    /// The instance stop-on-exit may stop: a rhema-derived name only —
    /// a stored target names an instance the user owns, never stopped.
    pub(crate) managed: Option<ManagedInstance>,
}

/// The backend of a managed instance — the stop argv's program.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ManagedKind {
    Docker,
    Lxd,
}

/// A rhema-derived instance this process may stop on the way out.
#[derive(Debug)]
pub(crate) struct ManagedInstance {
    kind: ManagedKind,
    name: String,
}

/// Builds the tool set a non-local choice runs the agent on: the
/// sandbox the choice names, seeded with the same discovered skills the
/// local path renders (and the user's SYSTEM.md, when one exists), then
/// wrapped in the built-in `bash`'s executor seam.
///
/// Startup failures are loud and name their key: a missing ssh host or
/// http url is a configuration error, not a silent fall-back to local —
/// only an unparseable *kind* falls back, at [`resolve_choice`].
pub async fn assemble(
    choice: SandboxChoice,
    profile: &Profile,
    workspace: &Path,
    secrets: Option<&dyn SecretStore>,
    base_system: Option<&str>,
) -> anyhow::Result<Sandboxed> {
    let host_workspace = workspace.to_string_lossy().into_owned();
    // The shared instance's coordination, `Some` for the mounted
    // choices: the lock is taken exclusive before any ensure — the
    // mutation window no two assemblies may share — and carried out on
    // `Sandboxed` for the session and the exit to use.
    let mut instance_lock = None;
    let mut managed = None;
    // Whether the skills ride the mounts; the seeded postures copy them
    // into the workspace instead.
    let mut mounted_skills = false;
    // What the choice resolved to — the same value `Sandboxed::target`
    // carries out, set by whichever arm runs.
    let target;
    let sandbox: Arc<dyn Sandbox> = match choice {
        // The caller skips this arm; the fast path constructs nothing.
        SandboxChoice::Local => anyhow::bail!("the local sandbox needs no assembly"),
        SandboxChoice::Docker => {
            let stored = sandbox_target(profile, "docker");
            let container = stored
                .clone()
                .unwrap_or_else(|| derived_container_name(workspace));
            target = container.clone();
            let (uid, gid) = workspace_owner(workspace)?;
            let skills = global_skills_dir();
            let agents = agents_skills_dir();
            let lock = InstanceLock::acquire("docker", &container)
                .map_err(|err| anyhow::anyhow!("the docker sandbox lock: {err}"))?;
            ensure_container(
                &container,
                &host_workspace,
                skills.as_deref(),
                agents.as_deref(),
            )
            .await?;
            instance_lock = Some(lock);
            managed = stored.is_none().then(|| ManagedInstance {
                kind: ManagedKind::Docker,
                name: container.clone(),
            });
            mounted_skills = true;
            // The workspace IS the bind mount: the sandbox-side path is
            // the host path itself, and FsRoot's file routes are plain
            // fs on the host side of the mount.
            let root = FsRoot::new(workspace.to_path_buf())
                .map_err(|err| anyhow::anyhow!("docker sandbox root: {err}"))?;
            Arc::new(
                DockerSandbox::new(container, root, host_workspace.clone()).with_user(uid, gid),
            )
        }
        SandboxChoice::Lxd => {
            let stored = sandbox_target(profile, "lxd");
            let instance = stored
                .clone()
                .unwrap_or_else(|| derived_lxd_name(workspace));
            target = instance.clone();
            let vm = profile
                .sandbox_instance_for("lxd")
                .map(|(_, stored)| stored.vm.unwrap_or(false))
                .unwrap_or_else(|| profile.sandbox_vm());
            let (uid, gid) = workspace_owner(workspace)?;
            let skills = global_skills_dir();
            let agents = agents_skills_dir();
            let lock = InstanceLock::acquire("lxd", &instance)
                .map_err(|err| anyhow::anyhow!("the lxd sandbox lock: {err}"))?;
            instance_lock = Some(lock);
            managed = stored.is_none().then(|| ManagedInstance {
                kind: ManagedKind::Lxd,
                name: instance.clone(),
            });
            mounted_skills = !vm;
            if vm {
                // A virtual machine takes no disk-device mount of a
                // host path: the attached posture, the workspace
                // carried in by the seed like ssh, exec through the
                // same lxc argv.
                ensure_lxd_instance(&instance, true, uid, gid).await?;
                Arc::new(
                    LxdSandbox::attached(&instance)
                        .with_workspace(&host_workspace)
                        .with_user(uid, gid),
                )
            } else {
                // The bind-mount posture: a disk device mirrors the
                // host workspace at its own path, so the sandbox-side
                // path is the host path itself and FsRoot's file
                // routes are plain fs on the host side.
                ensure_lxd_instance(&instance, false, uid, gid).await?;
                ensure_lxd_mount(
                    &instance,
                    &host_workspace,
                    skills.as_deref(),
                    agents.as_deref(),
                )
                .await?;
                let root = FsRoot::new(workspace.to_path_buf())
                    .map_err(|err| anyhow::anyhow!("lxd sandbox root: {err}"))?;
                Arc::new(
                    LxdSandbox::mounted(&instance, root, host_workspace.clone())
                        .with_user(uid, gid),
                )
            }
        }
        SandboxChoice::Ecrou => {
            // One-shot machines: nothing to stand up, provision, or
            // stop — every exec boots. The lock serializes the seed
            // the way it serializes every other assembly's mutation
            // window; the name is the workspace's derivation, stable
            // across sessions, never a stoppable instance.
            let image =
                sandbox_target(profile, "ecrou").unwrap_or_else(|| DEFAULT_IMAGE.to_owned());
            target = image.clone();
            let name = derived_ecrou_name(workspace);
            let lock = InstanceLock::acquire("ecrou", &name)
                .map_err(|err| anyhow::anyhow!("the ecrou sandbox lock: {err}"))?;
            instance_lock = Some(lock);
            // The skills ride the seed, not a read-only mount: the
            // strict snap ecrou ships in cannot see the config
            // directory a skills mount would name.
            let root = FsRoot::new(workspace.to_path_buf())
                .map_err(|err| anyhow::anyhow!("ecrou sandbox root: {err}"))?;
            // Rootfs state survives between commands by default: the
            // workspace's derived name keys ecrou's persisted upper,
            // so a session's installs are its next command's starting
            // point. `sandbox.persist: false` restores the one-command
            // rootfs, where only the workspace persists.
            let sandbox = EcrouSandbox::new(image, root, host_workspace.clone());
            let sandbox = if profile.sandbox_persist() {
                sandbox.with_persist(name)
            } else {
                sandbox
            };
            Arc::new(sandbox)
        }
        SandboxChoice::Ssh => {
            // Stored only, no env tier: a stray variable must not
            // silently move where commands run (rhema-config's stance).
            let host = sandbox_target(profile, "ssh").ok_or_else(|| {
                anyhow::anyhow!(
                    "the ssh sandbox needs a host: name an ssh instance or set \
                     `sandbox.host` in the profile"
                )
            })?;
            // The remote workspace root is the workspace path itself, so
            // the seeded skills land where the prompt's list points.
            target = host.clone();
            let sandbox = SshSandbox::new(host).with_workspace(host_workspace.clone());
            Arc::new(sandbox)
        }
        SandboxChoice::Http => {
            let url = sandbox_target(profile, "http").ok_or_else(|| {
                anyhow::anyhow!(
                    "the http sandbox needs a url: name an http instance or set \
                     RHEMA_SANDBOX_URL or the profile's `sandbox.url`"
                )
            })?;
            // One session per workspace, stable across runs: the
            // daemon multiplexes one root directory per session name.
            let session = workspace
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| "workspace".to_owned());
            // No token configured means no header value: a tokenless
            // daemon answers.
            let token = profile.sandbox_token(secrets).unwrap_or_default();
            target = url.clone();
            Arc::new(HttpSandbox::new(url, &session, &token))
        }
    };
    let workspace_string = match choice {
        SandboxChoice::Http => "/workspace".to_owned(),
        // The mount and ssh postures see the workspace at its own path.
        _ => host_workspace,
    };
    let agents = rhema_config::Config::agents_skills_dir().ok();
    let global = rhema_config::Config::skills_dir().ok();
    let (skills, owned_bodies) = discovered_skills(
        agents.as_deref(),
        global.as_deref(),
        workspace,
        mounted_skills,
        &workspace_string,
    );
    let bodies: Vec<(&str, &str)> = owned_bodies
        .iter()
        .map(|(name, body)| (name.as_str(), body.as_str()))
        .collect();
    ensure_seeded(&*sandbox, &workspace_string, &bodies, base_system).await?;
    // The assembly is done — devices, provision, and seed all landed
    // under the exclusive hold. Downgrade to the session's shared
    // posture; the lock rides `Sandboxed` out to the exit path.
    if let Some(lock) = instance_lock.as_ref() {
        lock.share()
            .map_err(|err| anyhow::anyhow!("the sandbox lock: {err}"))?;
    }
    let tools = Arc::new(ShellTools::with_executor(
        workspace_string.clone(),
        Arc::new(ShellSandbox::new(sandbox)),
    ));
    Ok(Sandboxed {
        choice,
        target,
        tools,
        workspace: workspace_string,
        skills,
        instance_lock,
        managed,
    })
}

impl Sandboxed {
    /// Puts the assembled tool set and the seeded skill list on a
    /// coding-agent config. Runs BEFORE the subagent wire: their inner
    /// shell inherits the executor installed here
    /// (`ShellTools::executor()`), while their definition discovery
    /// stays host-side.
    pub fn apply_tools(&self, config: &mut CodingAgentConfig) {
        config.tools = Some(self.tools.clone());
        config.skills = Some(self.skills.clone());
    }

    /// Puts the sandbox-side workspace path on a coding-agent config.
    /// Runs AFTER the subagent wire, so the path lands on the config
    /// the agent is finally built from.
    pub fn apply_workspace(&self, config: &mut CodingAgentConfig) {
        config.workspace = PathBuf::from(&self.workspace);
    }

    /// Stop-on-exit: when the session ran on a rhema-derived instance
    /// and this process is its lock's sole holder, stop it — no peer
    /// session remains to lose the sandbox under. Everything else (a
    /// stored target the user owns, a peer still holding the session
    /// open, a choice with no instance) leaves the instance as it
    /// stands; the lock's release is the drop. Best-effort by design:
    /// a failed stop leaves the instance for the next assembly to find,
    /// which the safe direction — up, never killed under a peer.
    pub fn shutdown(&mut self) {
        // RHEMA_SANDBOX_TRACE=1 makes every no-stop decision say why on
        // stderr; the failures already speak for themselves.
        let trace = std::env::var_os("RHEMA_SANDBOX_TRACE")
            .is_some_and(|value| value == "1" || value == "true");
        let Some(managed) = self.managed.take() else {
            self.instance_lock = None;
            if trace {
                let why = match self.choice {
                    SandboxChoice::Docker | SandboxChoice::Lxd => {
                        "the profile names this instance; leaving it running"
                    }
                    // One-shot machines and transports without an
                    // instance: nothing persists past the session.
                    _ => "nothing persists to stop",
                };
                eprintln!("sandbox: {why}");
            }
            return;
        };
        let Some(lock) = self.instance_lock.take() else {
            return;
        };
        if !lock.is_sole() {
            // A peer holds the session open: the instance is its
            // problem now, and this process's hold releases on the drop.
            if trace {
                eprintln!(
                    "sandbox: a peer still holds {}; leaving it running",
                    managed.name
                );
            }
            return;
        }
        if trace {
            eprintln!("sandbox: stopping {}", managed.name);
        }
        let program = match managed.kind {
            ManagedKind::Docker => "docker",
            ManagedKind::Lxd => "lxc",
        };
        let mut argv = vec!["stop".to_owned()];
        if managed.kind == ManagedKind::Lxd {
            // The bound on the clean-shutdown wait; past it LXD stops
            // the instance the hard way, and this exit does not wait
            // past that either.
            argv.push("--timeout=30".to_owned());
        }
        argv.push(managed.name.clone());
        // Best effort, but not silent: a stop that could not run — the
        // client missing, the daemon gone, a refused command — names
        // itself, so "the instance is still up" has an author on the
        // terminal the TUI just restored. The safe direction is
        // unchanged: up, never killed under a peer.
        match stop_instance(program, &argv) {
            Ok(true) => {
                if trace {
                    eprintln!("sandbox: stopped {}", managed.name);
                }
            }
            Ok(false) => eprintln!(
                "warning: the {} instance stays up: `{program} {}` failed",
                managed.name,
                argv.join(" ")
            ),
            Err(err) => eprintln!("warning: the {} instance stays up: {err}", managed.name),
        }
    }
}

/// Runs one stop command, output discarded. The bool answers "the
/// command ran and succeeded"; `Err` is the command never starting.
fn stop_instance(program: &str, argv: &[String]) -> std::io::Result<bool> {
    std::process::Command::new(program)
        .args(argv)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
}

/// The target a non-local `kind` execs in: the named instance's first
/// — the active pointer or the first stored of the kind — then the
/// section's own single-instance fact, the shape the map grew out of.
/// `None` means neither place has one and the caller derives or
/// refuses.
pub(crate) fn sandbox_target(profile: &Profile, kind: &str) -> Option<String> {
    profile
        .sandbox_instance_for(kind)
        .and_then(|(_, instance)| instance.target.clone())
        .map(|target| target.trim().to_owned())
        .filter(|target| !target.is_empty())
        .or_else(|| match kind {
            "docker" => profile.sandbox_container(),
            "lxd" => profile.sandbox_instance(),
            "ecrou" => profile.sandbox_image(),
            "ssh" => profile.sandbox_host(),
            "http" => profile.sandbox_url(),
            _ => None,
        })
}

/// The tool set a sandbox instance is prepared with: what the bash
/// tool's sessions lean on, installed once per instance so a stock
/// image arrives ready. The fd binary installs as `fdfind` on the
/// debian family; the script links the name the rest of the world
/// uses.
pub(crate) const SANDBOX_TOOLS: &str = "git ripgrep fd-find jq sd curl python3 build-essential";

/// The container name a workspace derives when none is stored:
/// `rhema-sandbox-<basename>-<hash>` — recognizable in `docker ps`,
/// unique per workspace path, and valid whatever the directory is
/// called (docker names take a narrow charset; the basename is
/// filtered and capped).
fn derived_container_name(workspace: &Path) -> String {
    let raw = workspace
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "workspace".to_owned());
    let mut base: String = raw
        .chars()
        .take(32)
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') {
                c
            } else {
                '-'
            }
        })
        .collect();
    if base.is_empty() || base.starts_with('.') || base.starts_with('-') {
        base.insert(0, 'w');
    }
    format!(
        "rhema-sandbox-{base}-{}",
        path_hash(&workspace.to_string_lossy())
    )
}

/// The instance name a workspace derives when none is stored: the
/// docker pattern — `rhema-sandbox-<basename>-<hash>` — recognizable
/// in `lxc list`, unique per workspace path, and valid whatever the
/// directory is called (LXD names take a narrow charset; the basename
/// is filtered and capped).
fn derived_lxd_name(workspace: &Path) -> String {
    derived_container_name(workspace)
}

/// The ecrou lock's name: the same derivation. There is no instance
/// to list — the name keys the assembly's cross-process lock only,
/// the seed's mutation window.
fn derived_ecrou_name(workspace: &Path) -> String {
    derived_container_name(workspace)
}

/// FNV-1a over the workspace path, hex — a hash pinned here so a
/// workspace's derived container name is stable across rhema builds
/// and Rust versions (std's `DefaultHasher` makes no such promise).
fn path_hash(path: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in path.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}
