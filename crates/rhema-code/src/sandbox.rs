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
use rhema_sandbox_ecrou::EcrouSandbox;
use rhema_sandbox_http::HttpSandbox;
use rhema_sandbox_lxd::LxdSandbox;
use rhema_sandbox_ssh::SshSandbox;

use crate::args::SandboxChoice;

/// The wall-clock bound on one `docker info` probe: the CLI answers in
/// well under a second when the daemon is up, and hangs or retries on
/// its own when it is not — a few seconds is the whole patience the
/// startup path can afford.
const DOCKER_PROBE_BUDGET: Duration = Duration::from_secs(3);

/// The same bound on one `lxc info` probe: the CLI answers fast when
/// the local daemon answers, and the startup path's patience is the
/// same few seconds.
const LXD_PROBE_BUDGET: Duration = Duration::from_secs(3);

/// The same bound on one `ecrou probe`: the CLI dlopens libkrun and
/// checks KVM, answering in well under a second when the host can
/// boot machines.
const ECROU_PROBE_BUDGET: Duration = Duration::from_secs(5);

/// The name the seeding marker file carries, under the workspace's
/// `.rhema` directory — the sandbox-side twin of the cloud frontends'
/// `workspace:seeded` meta row.
const SEED_MARKER: &str = ".seeded";

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
    /// The shell whose commands run in the sandbox; the subagents'
    /// inner shell inherits its executor.
    pub tools: Arc<ShellTools>,
    /// The workspace path as the sandbox side sees it: `/workspace`
    /// over http, the workspace's own path for the mount and ssh
    /// postures.
    pub workspace: String,
    /// The discovered skills; every `Skill::path` resolves inside the
    /// sandbox's filesystem — [`Self::workspace`] itself for the seeded
    /// postures, the workspace plus the read-only skills mount for the
    /// mounted ones — because the model reads the bodies through the
    /// sandbox's shell.
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
    let sandbox: Arc<dyn Sandbox> = match choice {
        // The caller skips this arm; the fast path constructs nothing.
        SandboxChoice::Local => anyhow::bail!("the local sandbox needs no assembly"),
        SandboxChoice::Docker => {
            let stored = sandbox_target(profile, "docker");
            let container = stored
                .clone()
                .unwrap_or_else(|| derived_container_name(workspace));
            let (uid, gid) = workspace_owner(workspace)?;
            let skills = global_skills_dir();
            let lock = InstanceLock::acquire("docker", &container)
                .map_err(|err| anyhow::anyhow!("the docker sandbox lock: {err}"))?;
            ensure_container(&container, &host_workspace, skills.as_deref())?;
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
            let vm = profile
                .sandbox_instance_for("lxd")
                .map(|(_, stored)| stored.vm.unwrap_or(false))
                .unwrap_or_else(|| profile.sandbox_vm());
            let (uid, gid) = workspace_owner(workspace)?;
            let skills = global_skills_dir();
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
                ensure_lxd_instance(&instance, true, uid, gid)?;
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
                ensure_lxd_instance(&instance, false, uid, gid)?;
                ensure_lxd_mount(&instance, &host_workspace, skills.as_deref())?;
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
            // stop — every exec boots, and the workspace share is the
            // whole persistence story. The lock serializes the seed
            // the way it serializes every other assembly's mutation
            // window; the name is the workspace's derivation, stable
            // across sessions, never a stoppable instance.
            let image = sandbox_target(profile, "ecrou").unwrap_or_else(|| "ubuntu".to_owned());
            let name = derived_ecrou_name(workspace);
            let lock = InstanceLock::acquire("ecrou", &name)
                .map_err(|err| anyhow::anyhow!("the ecrou sandbox lock: {err}"))?;
            instance_lock = Some(lock);
            // The skills ride the seed, not a read-only mount: the
            // strict snap ecrou ships in cannot see the config
            // directory a skills mount would name.
            let root = FsRoot::new(workspace.to_path_buf())
                .map_err(|err| anyhow::anyhow!("ecrou sandbox root: {err}"))?;
            Arc::new(EcrouSandbox::new(image, root, host_workspace.clone()))
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
            Arc::new(HttpSandbox::new(url, &session, &token))
        }
    };
    let workspace_string = match choice {
        SandboxChoice::Http => "/workspace".to_owned(),
        // The mount and ssh postures see the workspace at its own path.
        _ => host_workspace,
    };
    let (skills, owned_bodies) = discovered_skills(workspace, mounted_skills, &workspace_string);
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

/// The user's global skills directory, when it exists on disk — the
/// read-only `skills` mount's source. Absent, nothing is mounted and
/// discovery sees workspace skills only, as it always has.
fn global_skills_dir() -> Option<PathBuf> {
    let dir = rhema_config::Config::skills_dir().ok()?;
    dir.is_dir().then_some(dir)
}

/// The sandbox kinds this machine and this profile can run `bash`
/// under right now: the picker's rows, and the gates the startup
/// resolution filters every tier through. `local` needs no probing and
/// is always offered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SandboxAvailability {
    /// `docker info` answered within the budget.
    pub docker: bool,
    /// `lxc info` answered within the budget.
    pub lxd: bool,
    /// `ecrou probe` answered within the budget: KVM working,
    /// libkrun loadable, the cache writable.
    pub ecrou: bool,
    /// The `ssh` binary exists and the profile names a host.
    pub ssh: bool,
    /// The profile names a sandbox URL. Never network-probed: a daemon
    /// that is down must still be selectable, and the turn reports the
    /// failure in-band.
    pub http: bool,
}

impl SandboxAvailability {
    /// Probes this machine and profile: one `docker info` run, one
    /// `lxc info` run, one `PATH` scan for `ssh`, and the profile's
    /// own `sandbox` section. Bounded work — each CLI probe at most
    /// its budget — so startup waits seconds, never minutes.
    pub fn probe(profile: &Profile) -> Self {
        Self::from_parts(
            docker_info_answers(),
            lxd_info_answers(),
            ecrou_probe_answers(),
            binary_on_path("ssh"),
            profile,
        )
    }

    /// The offered set from injected machine facts: the testable seam
    /// of [`Self::probe`]. `ssh` is offered only when the binary
    /// exists AND the profile names a host; `http` only when a URL is
    /// set (`RHEMA_SANDBOX_URL` included).
    fn from_parts(
        docker_answers: bool,
        lxd_answers: bool,
        ecrou_answers: bool,
        ssh_present: bool,
        profile: &Profile,
    ) -> Self {
        Self {
            docker: docker_answers,
            lxd: lxd_answers,
            ecrou: ecrou_answers,
            ssh: ssh_present && profile.sandbox_host().is_some(),
            http: profile.sandbox_url().is_some(),
        }
    }

    /// Whether `choice` can run here: `local` always, the rest only
    /// when their probe answered.
    pub fn offers(self, choice: SandboxChoice) -> bool {
        match choice {
            SandboxChoice::Local => true,
            SandboxChoice::Docker => self.docker,
            SandboxChoice::Lxd => self.lxd,
            SandboxChoice::Ecrou => self.ecrou,
            SandboxChoice::Ssh => self.ssh,
            SandboxChoice::Http => self.http,
        }
    }

    /// The offered kinds, in the spec's canonical order — the picker's
    /// row order.
    pub fn offered(self) -> Vec<SandboxChoice> {
        [
            SandboxChoice::Local,
            SandboxChoice::Docker,
            SandboxChoice::Lxd,
            SandboxChoice::Ecrou,
            SandboxChoice::Ssh,
            SandboxChoice::Http,
        ]
        .into_iter()
        .filter(|choice| self.offers(*choice))
        .collect()
    }
}

/// Whether `lxc` answers at all: `lxc info` exiting zero within the
/// budget. A missing binary is the common "no" — treated exactly like
/// a refusing daemon.
fn lxd_info_answers() -> bool {
    let Ok(mut child) = std::process::Command::new("lxc")
        .arg("info")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    else {
        return false;
    };
    let deadline = Instant::now() + LXD_PROBE_BUDGET;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                return false;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(25)),
            Err(_) => return false,
        }
    }
}

/// Whether `ecrou` answers at all: `ecrou probe` exiting zero within
/// the budget. A missing binary is the common "no" — the host cannot
/// boot machines, and the picker says so.
fn ecrou_probe_answers() -> bool {
    let Ok(mut child) = std::process::Command::new("ecrou")
        .arg("probe")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    else {
        return false;
    };
    let deadline = Instant::now() + ECROU_PROBE_BUDGET;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                return false;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(25)),
            Err(_) => return false,
        }
    }
}

/// Whether `docker` answers at all: `docker info` exiting zero within
/// the budget. A missing binary is the common "no" — treated exactly
/// like a refusing daemon.
fn docker_info_answers() -> bool {
    let Ok(mut child) = std::process::Command::new("docker")
        .arg("info")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    else {
        return false;
    };
    let deadline = Instant::now() + DOCKER_PROBE_BUDGET;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(50)),
            // Timed out or the wait failed: kill and read as "no".
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return false;
            }
        }
    }
}

/// Whether a binary of this name sits on `PATH` — the ssh probe, which
/// must not spawn a process to learn the spawn would work. Both
/// spellings Windows installs answer.
fn binary_on_path(name: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path)
        .any(|dir| dir.join(name).is_file() || dir.join(format!("{name}.exe")).is_file())
}

/// Seeds a fresh sandbox workspace once: the skills (no git seed —
/// rhema-code has no repository to clone), then the user's SYSTEM.md
/// when one exists on disk, each written through the sandbox the way
/// the cloud frontends seed. The marker is written last, after a
/// successful seed, and checked first: the sandbox persists across
/// turns, and a re-seed would clone over live work.
async fn ensure_seeded(
    sandbox: &dyn Sandbox,
    workspace: &str,
    skills: &[(&str, &str)],
    base_system: Option<&str>,
) -> anyhow::Result<()> {
    if sandbox
        .read_file(&format!("{workspace}/.rhema/{SEED_MARKER}"))
        .await
        .is_ok()
    {
        return Ok(());
    }
    seed_workspace(sandbox, None, workspace, skills)
        .await
        .context("seeding the sandbox workspace")?;
    if let Some(system) = base_system {
        sandbox
            .write_file(&format!("{workspace}/.rhema/SYSTEM.md"), system.as_bytes())
            .await
            .context("seeding SYSTEM.md")?;
    }
    sandbox
        .write_file(&format!("{workspace}/.rhema/{SEED_MARKER}"), b"1")
        .await
        .context("writing the seed marker")?;
    Ok(())
}

/// The same discovery the local path's prompt renders — the global
/// config directory, then the workspace's `.rhema/skills`, project
/// shadowing global — paired with each body read host-side now. The
/// seeded postures' returned list names the rewritten, sandbox-side
/// paths and carries the bodies `seed_workspace` writes; a body that
/// cannot be read here would be unreadable there too, so it drops from
/// both. Mounted, the paths stand as discovered — the workspace's and
/// the skills directory's 1:1 mounts put both trees at their own paths
/// inside the instance — and no bodies come back: the mounts are the
/// delivery, nothing is copied, and a skill edited mid-session is
/// simply the new text the next read sees.
fn discovered_skills(
    workspace: &Path,
    mounted: bool,
    sandbox_workspace: &str,
) -> (Vec<Skill>, Vec<(String, String)>) {
    let global = rhema_config::Config::skills_dir().ok();
    let mut skills = Vec::new();
    let mut bodies = Vec::new();
    for skill in rhema_agent_tools::skills::discover(global.as_deref(), workspace) {
        let Ok(body) = std::fs::read_to_string(&skill.path) else {
            continue;
        };
        if mounted {
            skills.push(skill);
            continue;
        }
        skills.push(Skill {
            name: skill.name.clone(),
            description: skill.description,
            license: skill.license,
            path: PathBuf::from(format!(
                "{sandbox_workspace}/.rhema/skills/{}/SKILL.md",
                skill.name
            )),
        });
        bodies.push((skill.name, body));
    }
    (skills, bodies)
}

/// The target a non-local `kind` execs in: the named instance's first
/// — the active pointer or the first stored of the kind — then the
/// section's own single-instance fact, the shape the map grew out of.
/// `None` means neither place has one and the caller derives or
/// refuses.
fn sandbox_target(profile: &Profile, kind: &str) -> Option<String> {
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

/// The preparation script: marker-gated and idempotent. The marker
/// records the list it installed, so a list change re-runs the
/// install; a failed install writes no marker, so the next assembly
/// tries again. Runs as the instance's root — apt is system work,
/// and nothing here touches the mounted tree.
fn provision_script() -> String {
    format!(
        r#"set -e
m=/var/lib/rhema-sandbox/provisioned
w="{SANDBOX_TOOLS}"
[ -f "$m" ] && [ "$(cat "$m")" = "$w" ] && exit 0
export DEBIAN_FRONTEND=noninteractive
apt-get update -qq
apt-get install -y -qq $w
if command -v fdfind >/dev/null; then ln -sf "$(command -v fdfind)" /usr/local/bin/fd; fi
mkdir -p /var/lib/rhema-sandbox
echo "$w" > "$m"
"#
    )
}

/// The provision budget: apt on a fresh image takes a minute or two,
/// once.
const PROVISION_BUDGET_SECS: &str = "300";

/// Instances this process has already prepared or attempted — the
/// per-assembly marker check is one `exec` a turn, and the cache
/// makes it zero.
fn provision_attempted(backend: &str, name: &str) -> bool {
    use std::collections::HashSet;
    use std::sync::{Mutex, OnceLock};
    static ATTEMPTED: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    let attempted = ATTEMPTED.get_or_init(|| Mutex::new(HashSet::new()));
    let mut set = attempted
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    !set.insert(format!("{backend}/{name}"))
}

/// Runs the preparation script in the instance. A failure is a
/// degraded sandbox, not a dead one — the session runs and the tools
/// say what exists when probed — so the caller ignores the error and
/// the absent marker retries on the next assembly.
fn lxd_provision_argv(name: &str) -> Vec<String> {
    vec![
        "exec".to_owned(),
        name.to_owned(),
        "--".to_owned(),
        "timeout".to_owned(),
        "--kill-after=5".to_owned(),
        PROVISION_BUDGET_SECS.to_owned(),
        "bash".to_owned(),
        "-c".to_owned(),
        provision_script(),
    ]
}

fn docker_provision_argv(name: &str) -> Vec<String> {
    vec![
        "exec".to_owned(),
        name.to_owned(),
        "timeout".to_owned(),
        "--kill-after=5".to_owned(),
        PROVISION_BUDGET_SECS.to_owned(),
        "bash".to_owned(),
        "-c".to_owned(),
        provision_script(),
    ]
}

/// Stands the container up when it is not already running:
/// `docker inspect` answers, and a stopped or absent container is met
/// with `docker run -d … sleep infinity` — a bind mount of the
/// workspace at its own path, held open between runs. Posture, plainly:
/// process hygiene, not a security boundary — the container's commands
/// reach the same fs the local path would.
fn ensure_container(
    name: &str,
    workspace: &str,
    skills: Option<&std::path::Path>,
) -> anyhow::Result<()> {
    if let Some(mounts) = docker_mounts(name)? {
        if mounts_current(&mounts, workspace, skills) {
            // Present, mounts whole: started when it is not.
            if !container_running(name)? {
                let output = std::process::Command::new("docker")
                    .args(["start", name])
                    .output()
                    .map_err(|err| anyhow::anyhow!("docker start failed: {err}"))?;
                if !output.status.success() {
                    anyhow::bail!(
                        "docker start failed: {}",
                        String::from_utf8_lossy(&output.stderr).trim()
                    );
                }
            }
            prepare_instance("docker", name, &docker_provision_argv(name));
            return Ok(());
        }
        // Present with a stale mount set — a container from before
        // the skills bind, say. A start can never gain a mount, so
        // the container is recreated; running under the assembly's
        // exclusive lock, no peer session is live to lose it. What
        // is lost is container-internal state only (the provision's
        // packages), which the next prepare re-installs.
        let output = std::process::Command::new("docker")
            .args(["rm", "-f", name])
            .output()
            .map_err(|err| anyhow::anyhow!("docker rm failed: {err}"))?;
        if !output.status.success() {
            anyhow::bail!(
                "docker rm failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
    }
    let argv = docker_run_argv(name, workspace, skills);
    let refs: Vec<&str> = argv.iter().map(String::as_str).collect();
    let output = std::process::Command::new("docker")
        .args(&refs)
        .output()
        .map_err(|err| anyhow::anyhow!("docker run failed: {err}"))?;
    if !output.status.success() {
        anyhow::bail!(
            "docker run failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    prepare_instance("docker", name, &docker_provision_argv(name));
    Ok(())
}

/// The container's mounts as `docker inspect` prints them — `None` when
/// the container does not exist. A missing binary or refusing daemon
/// reads as absence: the `run` below fails with the same word.
fn docker_mounts(name: &str) -> anyhow::Result<Option<String>> {
    let output = std::process::Command::new("docker")
        .args(["inspect", "-f", "{{json .Mounts}}", name])
        .output()
        .map_err(|err| anyhow::anyhow!("docker inspect failed: {err}"))?;
    if !output.status.success() {
        return Ok(None);
    }
    Ok(Some(String::from_utf8_lossy(&output.stdout).into_owned()))
}

/// Whether the container's mounts cover this assembly's needs: the
/// workspace bind, and — when the global skills directory exists — the
/// read-only skills bind. Matched by the inspect JSON's `"Source"`
/// entries — the host side, the same string the run argv names — so a
/// workspace that is a prefix of some other mount's path cannot pass
/// for its own bind. A path the JSON's escaping mangles reads as
/// missing: the container is recreated with the correct mounts, the
/// slow path, never the wrong one.
fn mounts_current(mounts_json: &str, workspace: &str, skills: Option<&std::path::Path>) -> bool {
    if !mounts_json.contains(&source_entry(workspace)) {
        return false;
    }
    match skills {
        None => true,
        Some(dir) => mounts_json.contains(&source_entry(&dir.to_string_lossy())),
    }
}

/// The inspect JSON's source entry for one host path:
/// `"Source":"<path>"`.
fn source_entry(path: &str) -> String {
    format!(r#""Source":"{path}""#)
}

/// The run argv: `docker run -d --name <name> -v <ws>:<ws>
/// [-v <skills>:<skills>:ro] debian:trixie sleep infinity` — the
/// workspace bind is the container's filesystem story, the skills bind
/// rides read-only when the global directory exists.
fn docker_run_argv(name: &str, workspace: &str, skills: Option<&std::path::Path>) -> Vec<String> {
    let mut argv = vec![
        "run".to_owned(),
        "-d".to_owned(),
        "--name".to_owned(),
        name.to_owned(),
        "-v".to_owned(),
        bind_mount(workspace, false),
    ];
    if let Some(dir) = skills {
        argv.push("-v".to_owned());
        argv.push(bind_mount(&dir.to_string_lossy(), true));
    }
    argv.extend([
        "debian:trixie".to_owned(),
        "sleep".to_owned(),
        "infinity".to_owned(),
    ]);
    argv
}

/// One bind-mount argument: `src:src`, `:ro` when read-only — source
/// and destination agree, the same 1:1 posture the lxd device takes.
fn bind_mount(source: &str, readonly: bool) -> String {
    if readonly {
        format!("{source}:{source}:ro")
    } else {
        format!("{source}:{source}")
    }
}

/// Prepares an instance once per process: the marker gate inside the
/// script makes the remote side idempotent, and this gate keeps the
/// check itself from running a turn after the first.
fn prepare_instance(backend: &str, name: &str, argv: &[String]) {
    if provision_attempted(backend, name) {
        return;
    }
    let refs: Vec<&str> = argv.iter().map(String::as_str).collect();
    let output = std::process::Command::new("timeout")
        .args(["--kill-after=5", PROVISION_BUDGET_SECS])
        .args(refs)
        .output();
    let _ = output; // a failed preparation is the next assembly's retry
}

/// Whether `docker` answers that the container is up. A missing or
/// refusing `docker` fails here and names it: standing the container up
/// would only fail with the same word.
fn container_running(name: &str) -> anyhow::Result<bool> {
    let output = std::process::Command::new("docker")
        .args(["inspect", "-f", "{{.State.Running}}", name])
        .output()
        .map_err(|err| anyhow::anyhow!("docker inspect failed: {err}"))?;
    Ok(output.status.success() && String::from_utf8_lossy(&output.stdout).trim() == "true")
}

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

/// Stands the LXD instance up when it is not already running:
/// `lxc list` answers whether it exists, an existing one is started,
/// an absent one is launched — `--vm` for a virtual machine, else a
/// container — from the stock `ubuntu:24.04` image. First launches
/// wait on the image download the same way the docker path waits on
/// its pull; the assembly is the one place that patience is spent.
/// The idmap entry the workspace's owner needs: the tree on the
/// device belongs to the host's uid, and an unprivileged container's
/// default map turns that into nobody — the mount attaches and no
/// one inside can read it. `both <uid> <gid>` carries the pair in
/// both directions, so exec's `--user` is the tree's own owner.
fn wanted_idmap(uid: u32, gid: u32) -> String {
    format!("both {uid} {gid}")
}

/// The launch argv: `launch [--vm] ubuntu:24.04 <name> -c
/// raw.idmap=both <uid> <gid>` — the map set at birth, when it
/// applies on the first boot for free.
fn lxd_launch_argv(name: &str, vm: bool, uid: u32, gid: u32) -> Vec<String> {
    let mut argv = vec![
        "launch".to_owned(),
        "ubuntu:24.04".to_owned(),
        name.to_owned(),
        "-c".to_owned(),
        format!("raw.idmap={}", wanted_idmap(uid, gid)),
    ];
    if vm {
        argv.insert(1, "--vm".to_owned());
    }
    argv
}

/// The workspace owner: the uid/gid the tree answers to on the host.
/// The instance's map and exec's identity are both this pair.
fn workspace_owner(workspace: &std::path::Path) -> anyhow::Result<(u32, u32)> {
    use std::os::unix::fs::MetadataExt;
    let meta = std::fs::metadata(workspace)
        .map_err(|err| anyhow::anyhow!("the lxd sandbox needs the workspace's owner: {err}"))?;
    Ok((meta.uid(), meta.gid()))
}

fn lxc_output(args: &[&str]) -> anyhow::Result<std::process::Output> {
    std::process::Command::new("lxc")
        .args(args)
        .output()
        .map_err(|err| anyhow::anyhow!("lxc {} failed: {err}", args.join(" ")))
}

fn ensure_lxd_instance(name: &str, vm: bool, uid: u32, gid: u32) -> anyhow::Result<()> {
    let output = lxc_output(&["list", "--format", "csv", "-c", "s", name])?;
    if !output.status.success() {
        anyhow::bail!(
            "lxc list failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let state = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    let want = wanted_idmap(uid, gid);
    if state == "RUNNING" {
        prepare_instance("lxd", name, &lxd_provision_argv(name));
        // An instance launched before the map existed stays
        // unreadable until it carries the entry: repair in place —
        // the setting lands on the next boot, so this restarts once
        // and never again.
        let have = idmap_of(name)?;
        if have != want {
            let set = ["config", "set", name, "raw.idmap", &want];
            let output = lxc_output(&set)?;
            if !output.status.success() {
                anyhow::bail!(
                    "lxc config set failed: {}",
                    String::from_utf8_lossy(&output.stderr).trim()
                );
            }
            let output = lxc_output(&["restart", name])?;
            if !output.status.success() {
                anyhow::bail!(
                    "lxc restart failed: {}",
                    String::from_utf8_lossy(&output.stderr).trim()
                );
            }
        }
        return Ok(());
    }
    if state.is_empty() {
        // Absent: launch it, virtual machine or container, the map
        // set at birth.
        let argv = lxd_launch_argv(name, vm, uid, gid);
        let refs: Vec<&str> = argv.iter().map(String::as_str).collect();
        let output = lxc_output(&refs)?;
        if !output.status.success() {
            anyhow::bail!(
                "lxc launch failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        prepare_instance("lxd", name, &lxd_provision_argv(name));
        return Ok(());
    }
    // Present but stopped: set the map while it is down, so one
    // boot carries it.
    if idmap_of(name)? != want {
        let set = ["config", "set", name, "raw.idmap", &want];
        let output = lxc_output(&set)?;
        if !output.status.success() {
            anyhow::bail!(
                "lxc config set failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
    }
    let output = lxc_output(&["start", name])?;
    if !output.status.success() {
        anyhow::bail!(
            "lxc start failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    prepare_instance("lxd", name, &lxd_provision_argv(name));
    Ok(())
}

/// The instance's raw.idmap, empty when unset.
fn idmap_of(name: &str) -> anyhow::Result<String> {
    let output = lxc_output(&["config", "get", name, "raw.idmap"])?;
    if !output.status.success() {
        anyhow::bail!(
            "lxc config get failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

/// Adds the workspace's `disk` device — and the global skills
/// directory's, read-only, when it exists: `lxc config device add
/// <name> <device> disk source=<dir> path=<dir>` — the 1:1 mirrors
/// that make the sandbox-side paths the host paths themselves.
/// Containers only; a virtual machine takes no host-path mount and
/// rides the attached posture instead.
fn ensure_lxd_mount(
    name: &str,
    workspace: &str,
    skills: Option<&std::path::Path>,
) -> anyhow::Result<()> {
    let devices = lxd_devices(name)?;
    lxd_device(name, &devices, "workspace", workspace, false)?;
    if let Some(dir) = skills {
        // Re-shown: the first add changed the set this check reads.
        let devices = lxd_devices(name)?;
        let dir = dir.to_string_lossy().into_owned();
        lxd_device(name, &devices, "skills", &dir, true)?;
    }
    Ok(())
}

/// The instance's device set as `lxc config device show` prints it.
fn lxd_devices(name: &str) -> anyhow::Result<String> {
    let output = std::process::Command::new("lxc")
        .args(["config", "device", "show", name])
        .output()
        .map_err(|err| anyhow::anyhow!("lxc config device show failed: {err}"))?;
    if !output.status.success() {
        anyhow::bail!(
            "lxc config device show failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// One 1:1 `disk` device, added when the shown set lacks it. A failed
/// add re-shows before failing: a device another hand added in the gap
/// is success, not error.
fn lxd_device(
    name: &str,
    devices: &str,
    device: &str,
    dir: &str,
    readonly: bool,
) -> anyhow::Result<()> {
    if devices.contains(&format!("{device}:")) {
        return Ok(());
    }
    let argv = lxd_device_argv(name, device, dir, readonly);
    let refs: Vec<&str> = argv.iter().map(String::as_str).collect();
    let output = std::process::Command::new("lxc")
        .args(&refs)
        .output()
        .map_err(|err| anyhow::anyhow!("lxc config device add failed: {err}"))?;
    if !output.status.success() && !lxd_devices(name)?.contains(&format!("{device}:")) {
        anyhow::bail!(
            "lxc config device add failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

/// The add argv: `config device add <name> <device> disk
/// source=<dir> path=<dir> [readonly=true]` — read-only for the
/// skills device, the user's own config the instance must read but
/// never write.
fn lxd_device_argv(name: &str, device: &str, dir: &str, readonly: bool) -> Vec<String> {
    let mut argv = vec![
        "config".to_owned(),
        "device".to_owned(),
        "add".to_owned(),
        name.to_owned(),
        device.to_owned(),
        "disk".to_owned(),
        format!("source={dir}"),
        format!("path={dir}"),
    ];
    if readonly {
        argv.push("readonly=true".to_owned());
    }
    argv
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

#[cfg(test)]
mod provision {
    use super::*;

    #[test]
    fn the_lxd_provision_argv_runs_the_script_under_a_budget() {
        let argv = lxd_provision_argv("box");
        assert_eq!(&argv[..4], &["exec", "box", "--", "timeout"]);
        assert_eq!(&argv[4..7], &["--kill-after=5", "300", "bash"]);
        assert_eq!(argv[7], "-c");
        assert_eq!(argv[8], provision_script());
    }

    #[test]
    fn the_docker_provision_argv_runs_the_script_under_a_budget() {
        let argv = docker_provision_argv("box");
        assert_eq!(&argv[..3], &["exec", "box", "timeout"]);
        assert_eq!(argv.last().unwrap(), &provision_script());
    }

    #[test]
    fn the_script_is_marker_gated_and_installs_the_tool_list() {
        let script = provision_script();
        assert!(script.contains("provisioned"));
        assert!(script.contains(SANDBOX_TOOLS));
        assert!(script.contains("apt-get install"));
        assert!(
            script.contains("fdfind"),
            "fd's debian name is linked to the common one"
        );
    }
}

#[cfg(test)]
mod lxd_launch {
    use super::*;

    #[test]
    fn the_launch_argv_sets_the_owners_map_at_birth() {
        assert_eq!(
            lxd_launch_argv("rhema-sandbox-dev", false, 1000, 1000),
            vec![
                "launch",
                "ubuntu:24.04",
                "rhema-sandbox-dev",
                "-c",
                "raw.idmap=both 1000 1000",
            ]
        );
    }

    #[test]
    fn the_vm_flag_precedes_the_image() {
        assert_eq!(
            lxd_launch_argv("vm0", true, 33, 33),
            vec![
                "launch",
                "--vm",
                "ubuntu:24.04",
                "vm0",
                "-c",
                "raw.idmap=both 33 33",
            ]
        );
    }

    #[test]
    fn the_idmap_names_the_pair_both_ways() {
        assert_eq!(wanted_idmap(1000, 1000), "both 1000 1000");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::args::SandboxChoice;
    use rhema_sandbox::mock::ScriptedSandbox;

    #[test]
    fn the_choice_resolves_from_the_layered_kind() {
        // The env tier is injected as the value Profile::sandbox_kind
        // would return with RHEMA_SANDBOX set: it wins over the stored
        // kind.
        assert_eq!(choice_from_kind(Some("docker")), SandboxChoice::Docker);
        assert_eq!(choice_from_kind(Some("http")), SandboxChoice::Http);
        // An explicit local, nothing set, and an unparseable spelling
        // all read as the fast path — the zero-flag invocation never
        // breaks because of a typo'd config.
        assert_eq!(choice_from_kind(Some("local")), SandboxChoice::Local);
        assert_eq!(choice_from_kind(None), SandboxChoice::Local);
        assert_eq!(choice_from_kind(Some("dockre")), SandboxChoice::Local);
    }

    #[test]
    fn resolve_reads_the_profile_through_the_real_env() {
        // The profile path runs only when the suite exports nothing:
        // the real environment decides, and the pure test above carries
        // the env-wins coverage (the edition forbids writing the var).
        if std::env::var_os("RHEMA_SANDBOX").is_some() {
            return;
        }
        let profile_with_kind = |kind: &str| Profile {
            sandbox: Some(rhema_config::SandboxSettings {
                kind: Some(kind.into()),
                ..rhema_config::SandboxSettings::default()
            }),
            ..Profile::default()
        };
        assert_eq!(
            resolve_choice(&profile_with_kind("ssh")),
            SandboxChoice::Ssh
        );
        assert_eq!(
            resolve_choice(&profile_with_kind("typo")),
            SandboxChoice::Local
        );
        assert_eq!(resolve_choice(&Profile::default()), SandboxChoice::Local);
    }

    #[tokio::test]
    async fn seeding_writes_skills_system_and_the_marker() {
        let sandbox = ScriptedSandbox::new();
        ensure_seeded(
            &sandbox,
            "/workspace",
            &[("review", "# review")],
            Some("# system"),
        )
        .await
        .expect("the seed lands");
        let files = sandbox.files();
        assert_eq!(
            files
                .get("/workspace/.rhema/skills/review/SKILL.md")
                .map(Vec::as_slice),
            Some(b"# review".as_slice())
        );
        assert_eq!(
            files.get("/workspace/.rhema/SYSTEM.md").map(Vec::as_slice),
            Some(b"# system".as_slice())
        );
        assert!(files.contains_key("/workspace/.rhema/.seeded"));
        assert!(sandbox.seeds().is_empty(), "rhema-code has no git seed");
    }

    /// One workspace skill on disk, for the discovery tests: the
    /// frontmatter the parser answers to.
    fn workspace_with_skill() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        let skill = dir.path().join(".rhema").join("skills").join("review");
        std::fs::create_dir_all(&skill).expect("the skill directory");
        std::fs::write(
            skill.join("SKILL.md"),
            "---\ndescription: review a diff\n---\n# review\n",
        )
        .expect("the skill lands");
        dir
    }

    #[test]
    fn mounted_discovery_keeps_the_host_paths_and_seeds_nothing() {
        let dir = workspace_with_skill();
        let ws = dir.path().to_path_buf();
        let (skills, bodies) = discovered_skills(&ws, true, &ws.to_string_lossy());
        assert!(bodies.is_empty(), "the mounts are the delivery: {bodies:?}");
        let skill = skills
            .iter()
            .find(|skill| skill.name == "review")
            .expect("the workspace skill is discovered");
        assert_eq!(
            skill.path,
            ws.join(".rhema")
                .join("skills")
                .join("review")
                .join("SKILL.md"),
            "the 1:1 mount puts the tree at its own path"
        );
    }

    #[test]
    fn seeded_discovery_rewrites_the_paths_and_carries_the_bodies() {
        let dir = workspace_with_skill();
        let ws = dir.path().to_path_buf();
        let (skills, bodies) = discovered_skills(&ws, false, "/workspace");
        let skill = skills
            .iter()
            .find(|skill| skill.name == "review")
            .expect("the workspace skill is discovered");
        assert_eq!(
            skill.path,
            PathBuf::from("/workspace/.rhema/skills/review/SKILL.md")
        );
        assert!(
            bodies
                .iter()
                .any(|(name, body)| name == "review" && body.contains("# review")),
            "the seeded posture carries the bodies: {bodies:?}"
        );
    }

    #[test]
    fn the_run_argv_binds_the_workspace_and_the_read_only_skills() {
        assert_eq!(
            docker_run_argv("box", "/home/me/proj", None),
            vec![
                "run",
                "-d",
                "--name",
                "box",
                "-v",
                "/home/me/proj:/home/me/proj",
                "debian:trixie",
                "sleep",
                "infinity",
            ]
            .into_iter()
            .map(String::from)
            .collect::<Vec<_>>()
        );
        let skills = Path::new("/h/.config/rhema/skills");
        assert_eq!(
            docker_run_argv("box", "/home/me/proj", Some(skills)),
            vec![
                "run",
                "-d",
                "--name",
                "box",
                "-v",
                "/home/me/proj:/home/me/proj",
                "-v",
                "/h/.config/rhema/skills:/h/.config/rhema/skills:ro",
                "debian:trixie",
                "sleep",
                "infinity",
            ]
            .into_iter()
            .map(String::from)
            .collect::<Vec<_>>()
        );
    }

    #[test]
    fn the_stop_answers_whether_the_command_ran_and_succeeded() {
        // The PATH's `true` and `false` are the cheapest stand-ins for
        // `docker stop`/`lxc stop`: the three outcomes a shutdown can
        // see — clean, failed, never started — each have a shape.
        assert_eq!(stop_instance("true", &[]).ok(), Some(true));
        assert_eq!(stop_instance("false", &[]).ok(), Some(false));
        assert!(stop_instance("rhema-no-such-program", &[]).is_err());
    }

    #[test]
    fn a_mount_set_missing_the_skills_bind_is_stale() {
        let mounts = r#"[{"Type":"bind","Source":"/home/me/proj","Destination":"/home/me/proj"}]"#;
        assert!(mounts_current(mounts, "/home/me/proj", None));
        assert!(!mounts_current(
            mounts,
            "/home/me/proj",
            Some(Path::new("/h/.config/rhema/skills"))
        ));
        let whole = r#"[{"Source":"/home/me/proj"},{"Source":"/h/.config/rhema/skills"}]"#;
        assert!(mounts_current(
            whole,
            "/home/me/proj",
            Some(Path::new("/h/.config/rhema/skills"))
        ));
        // A workspace that is a prefix of another mount's path does not
        // pass for its own bind.
        assert!(!mounts_current(
            r#"[{"Source":"/home/me/proj2"}]"#,
            "/home/me/proj",
            None
        ));
    }

    #[test]
    fn the_lxd_device_argv_names_the_seam_and_the_read_only_bit() {
        assert_eq!(
            lxd_device_argv("box", "workspace", "/home/me/proj", false),
            vec![
                "config",
                "device",
                "add",
                "box",
                "workspace",
                "disk",
                "source=/home/me/proj",
                "path=/home/me/proj",
            ]
            .into_iter()
            .map(String::from)
            .collect::<Vec<_>>()
        );
        assert_eq!(
            lxd_device_argv("box", "skills", "/h/.config/rhema/skills", true),
            vec![
                "config",
                "device",
                "add",
                "box",
                "skills",
                "disk",
                "source=/h/.config/rhema/skills",
                "path=/h/.config/rhema/skills",
                "readonly=true",
            ]
            .into_iter()
            .map(String::from)
            .collect::<Vec<_>>()
        );
    }

    #[tokio::test]
    async fn the_marker_stops_a_second_seed() {
        let sandbox = ScriptedSandbox::new();
        ensure_seeded(
            &sandbox,
            "/workspace",
            &[("review", "# review")],
            Some("# system"),
        )
        .await
        .expect("the seed lands");
        // A later run with different skills must not re-clone over live
        // work: the marker short-circuits before any write.
        ensure_seeded(&sandbox, "/workspace", &[("other", "# other")], None)
            .await
            .expect("the marker reads as seeded");
        let files = sandbox.files();
        assert!(!files.contains_key("/workspace/.rhema/skills/other/SKILL.md"));
        assert_eq!(
            files.get("/workspace/.rhema/SYSTEM.md").map(Vec::as_slice),
            Some(b"# system".as_slice())
        );
    }

    #[tokio::test]
    async fn no_system_file_seeds_no_system_md() {
        let sandbox = ScriptedSandbox::new();
        ensure_seeded(&sandbox, "/workspace", &[], None)
            .await
            .expect("the bare seed lands");
        let files = sandbox.files();
        assert!(!files.contains_key("/workspace/.rhema/SYSTEM.md"));
        assert!(files.contains_key("/workspace/.rhema/.seeded"));
    }

    #[test]
    fn the_derived_container_name_is_valid_stable_and_distinct() {
        let workspace = Path::new("/home/me/my proj");
        let name = derived_container_name(workspace);
        // Docker names: alphanumerics, dots, dashes, underscores.
        assert!(
            name.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_')),
            "{name}"
        );
        assert!(name.starts_with("rhema-sandbox-my-proj-"), "{name}");
        // Stable across calls, distinct across paths.
        assert_eq!(name, derived_container_name(workspace));
        assert_ne!(name, derived_container_name(Path::new("/home/me/other")));
        // A hostile basename still yields a name docker accepts.
        let hostile = derived_container_name(Path::new("/srv/.hidden-rép"));
        assert!(
            hostile
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_')),
            "{hostile}"
        );
        assert_eq!(
            hostile,
            derived_container_name(Path::new("/srv/.hidden-rép"))
        );
    }

    #[test]
    fn the_path_hash_is_pinned() {
        // Pinned so the derived names outlive toolchain changes: a
        // value drift here would orphan every standing container.
        assert_eq!(path_hash("/home/me/proj"), "2d872b17b24e117e");
    }

    /// A profile whose `sandbox` section carries exactly these fields.
    fn sandbox_profile(host: Option<&str>, url: Option<&str>) -> Profile {
        Profile {
            sandbox: Some(rhema_config::SandboxSettings {
                host: host.map(str::to_owned),
                url: url.map(str::to_owned),
                ..rhema_config::SandboxSettings::default()
            }),
            ..Profile::default()
        }
    }

    #[test]
    fn local_is_offered_on_a_machine_that_offers_nothing_else() {
        let none = SandboxAvailability::default();
        assert!(none.offers(SandboxChoice::Local));
        assert!(!none.offers(SandboxChoice::Docker));
        assert!(!none.offers(SandboxChoice::Lxd));
        assert!(!none.offers(SandboxChoice::Ecrou));
        assert!(!none.offers(SandboxChoice::Ssh));
        assert!(!none.offers(SandboxChoice::Http));
        assert_eq!(none.offered(), vec![SandboxChoice::Local]);
    }

    #[test]
    fn the_offered_list_keeps_the_canonical_order() {
        let all = SandboxAvailability {
            docker: true,
            lxd: true,
            ecrou: true,
            ssh: true,
            http: true,
        };
        assert_eq!(
            all.offered(),
            vec![
                SandboxChoice::Local,
                SandboxChoice::Docker,
                SandboxChoice::Lxd,
                SandboxChoice::Ecrou,
                SandboxChoice::Ssh,
                SandboxChoice::Http,
            ]
        );
    }

    #[test]
    fn ssh_needs_both_the_binary_and_a_host() {
        let host = sandbox_profile(Some("deploy@box"), None);
        assert!(
            SandboxAvailability::from_parts(true, false, false, true, &host)
                .offers(SandboxChoice::Ssh)
        );
        // A configured host with no ssh binary is not offered.
        assert!(
            !SandboxAvailability::from_parts(true, false, false, false, &host)
                .offers(SandboxChoice::Ssh)
        );
        // A binary with nothing configured is not either.
        let bare = sandbox_profile(None, None);
        assert!(
            !SandboxAvailability::from_parts(true, false, false, true, &bare)
                .offers(SandboxChoice::Ssh)
        );
    }

    #[test]
    fn http_is_offered_on_a_stored_url_alone() {
        // No probe runs: a daemon that is down must still be
        // selectable, and the turn reports the failure in-band.
        let url = sandbox_profile(None, Some("http://box:7331"));
        assert!(
            SandboxAvailability::from_parts(false, false, false, false, &url)
                .offers(SandboxChoice::Http)
        );
        let bare = sandbox_profile(None, None);
        assert!(
            !SandboxAvailability::from_parts(false, false, false, false, &bare)
                .offers(SandboxChoice::Http)
        );
    }

    #[test]
    fn the_instance_map_outranks_the_section_fact_for_a_target() {
        // No instance stored: the section's single-instance fact is
        // the fallback.
        let mut profile = sandbox_profile(None, None);
        profile.sandbox = Some(rhema_config::SandboxSettings {
            container: Some("section-dev".to_owned()),
            ..rhema_config::SandboxSettings::default()
        });
        assert_eq!(
            sandbox_target(&profile, "docker").as_deref(),
            Some("section-dev")
        );
        // A stored instance outranks it, and whitespace trims.
        profile.place_sandbox_instance(
            "dev",
            rhema_config::SandboxInstance {
                kind: "docker".to_owned(),
                target: Some(" map-dev ".to_owned()),
                vm: None,
            },
        );
        assert_eq!(
            sandbox_target(&profile, "docker").as_deref(),
            Some("map-dev"),
            "the instance wins, trimmed"
        );
        // A kind with no instance and no fact: None, the caller
        // derives or refuses.
        assert_eq!(sandbox_target(&profile, "lxd"), None);
        // A section fact answers for that kind while no instance of it
        // is stored.
        profile.sandbox.as_mut().unwrap().instance = Some("section-vm".to_owned());
        assert_eq!(
            sandbox_target(&profile, "lxd").as_deref(),
            Some("section-vm")
        );
    }

    #[test]
    fn docker_passes_its_machine_fact_through_untouched() {
        let bare = sandbox_profile(None, None);
        assert!(
            SandboxAvailability::from_parts(true, false, false, false, &bare)
                .offers(SandboxChoice::Docker)
        );
        assert!(
            !SandboxAvailability::from_parts(false, false, false, false, &bare)
                .offers(SandboxChoice::Docker)
        );
    }

    #[test]
    fn the_assembly_applies_tools_before_and_workspace_after_the_wire_order() {
        // Nothing here executes: the apply step only clones Arcs and
        // copies strings onto the config, so the plain local shell
        // stands in for an assembled one.
        let assembled = Sandboxed {
            choice: SandboxChoice::Http,
            tools: Arc::new(ShellTools::new("/workspace")),
            workspace: "/workspace".to_owned(),
            skills: vec![Skill {
                name: "review".into(),
                description: "trigger".into(),
                license: None,
                path: PathBuf::from("/workspace/.rhema/skills/review/SKILL.md"),
            }],
            instance_lock: None,
            managed: None,
        };
        let mut config = CodingAgentConfig::new("m", Path::new("/host/ws"));
        assert!(config.tools.is_none());
        assembled.apply_tools(&mut config);
        assert_eq!(
            config.tools.as_ref().map(|t| t.workspace()),
            Some(Path::new("/workspace"))
        );
        assert_eq!(config.skills.as_ref().map(Vec::len), Some(1));
        assert_eq!(
            config.workspace,
            Path::new("/host/ws"),
            "workspace waits for the wire"
        );
        assembled.apply_workspace(&mut config);
        assert_eq!(config.workspace, Path::new("/workspace"));
    }
}
