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

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Context;
use rhema_agent_tools::{ShellTools, Skill};
use rhema_config::Profile;
use rhema_config::secrets::SecretStore;
use rhema_sandbox::SshSandbox;
use rhema_sandbox::http::HttpSandbox;
use rhema_sandbox::{DockerSandbox, FsRoot, Sandbox, ShellSandbox, seed_workspace};

use crate::args::SandboxChoice;

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
    /// The shell whose commands run in the sandbox; the subagents'
    /// inner shell inherits its executor.
    pub tools: Arc<ShellTools>,
    /// The workspace path as the sandbox side sees it: `/workspace`
    /// over http, the workspace's own path for the mount and ssh
    /// postures.
    pub workspace: String,
    /// The discovered skills, seeded into the sandbox; every
    /// `Skill::path` resolves inside [`Self::workspace`], because the
    /// model reads the bodies through the sandbox's shell.
    pub skills: Vec<Skill>,
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
    let sandbox: Arc<dyn Sandbox> = match choice {
        // The caller skips this arm; the fast path constructs nothing.
        SandboxChoice::Local => anyhow::bail!("the local sandbox needs no assembly"),
        SandboxChoice::Docker => {
            let container = profile
                .sandbox_container()
                .unwrap_or_else(|| derived_container_name(workspace));
            ensure_container(&container, &host_workspace)?;
            // The workspace IS the bind mount: the sandbox-side path is
            // the host path itself, and FsRoot's file routes are plain
            // fs on the host side of the mount.
            let root = FsRoot::new(workspace.to_path_buf())
                .map_err(|err| anyhow::anyhow!("docker sandbox root: {err}"))?;
            Arc::new(DockerSandbox::new(container, root, host_workspace.clone()))
        }
        SandboxChoice::Ssh => {
            // Stored only, no env tier: a stray variable must not
            // silently move where commands run (rhema-config's stance).
            let host = profile.sandbox_host().ok_or_else(|| {
                anyhow::anyhow!("the ssh sandbox needs a host: set `sandbox.host` in the profile")
            })?;
            // The remote workspace root is the workspace path itself, so
            // the seeded skills land where the prompt's list points.
            let sandbox = SshSandbox::new(host).with_workspace(host_workspace.clone());
            Arc::new(sandbox)
        }
        SandboxChoice::Http => {
            let url = profile.sandbox_url().ok_or_else(|| {
                anyhow::anyhow!(
                    "the http sandbox needs a url: set RHEMA_SANDBOX_URL or the \
                     profile's `sandbox.url`"
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
    let (skills, owned_bodies) = discovered_skills(workspace, &workspace_string);
    let bodies: Vec<(&str, &str)> = owned_bodies
        .iter()
        .map(|(name, body)| (name.as_str(), body.as_str()))
        .collect();
    ensure_seeded(&*sandbox, &workspace_string, &bodies, base_system).await?;
    let tools = Arc::new(ShellTools::with_executor(
        workspace_string.clone(),
        Arc::new(ShellSandbox::new(sandbox)),
    ));
    Ok(Sandboxed {
        tools,
        workspace: workspace_string,
        skills,
    })
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
/// returned list names the seeded, sandbox-side paths; a body that
/// cannot be read here would be unreadable there too, so it drops from
/// both.
fn discovered_skills(
    workspace: &Path,
    sandbox_workspace: &str,
) -> (Vec<Skill>, Vec<(String, String)>) {
    let global = rhema_config::Config::skills_dir().ok();
    let mut skills = Vec::new();
    let mut bodies = Vec::new();
    for skill in rhema_agent_tools::skills::discover(global.as_deref(), workspace) {
        let Ok(body) = std::fs::read_to_string(&skill.path) else {
            continue;
        };
        skills.push(Skill {
            name: skill.name.clone(),
            description: skill.description,
            path: PathBuf::from(format!(
                "{sandbox_workspace}/.rhema/skills/{}/SKILL.md",
                skill.name
            )),
        });
        bodies.push((skill.name, body));
    }
    (skills, bodies)
}

/// Stands the container up when it is not already running:
/// `docker inspect` answers, and a stopped or absent container is met
/// with `docker run -d … sleep infinity` — a bind mount of the
/// workspace at its own path, held open between runs. Posture, plainly:
/// process hygiene, not a security boundary — the container's commands
/// reach the same fs the local path would.
fn ensure_container(name: &str, workspace: &str) -> anyhow::Result<()> {
    if container_running(name)? {
        return Ok(());
    }
    let argv = [
        "run",
        "-d",
        "--name",
        name,
        "-v",
        &format!("{workspace}:{workspace}"),
        "debian:trixie",
        "sleep",
        "infinity",
    ];
    let output = std::process::Command::new("docker")
        .args(argv)
        .output()
        .map_err(|err| anyhow::anyhow!("docker run failed: {err}"))?;
    if !output.status.success() {
        anyhow::bail!(
            "docker {} failed: {}",
            argv.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
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
}
