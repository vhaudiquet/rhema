//! The skills' ride into the sandbox: host discovery, the per-posture
//! mount or seed decision, and the seed marker that keeps a re-seed
//! from cloning over live work.

use super::*;

/// The name the seeding marker file carries, under the workspace's
/// `.rhema` directory — the sandbox-side twin of the cloud frontends'
/// `workspace:seeded` meta row.
const SEED_MARKER: &str = ".seeded";

/// The user's global skills directory, when it exists on disk — the
/// read-only `skills` mount's source. Absent, nothing is mounted and
/// discovery sees workspace skills only, as it always has.
pub(crate) fn global_skills_dir() -> Option<PathBuf> {
    let dir = rhema_config::Config::skills_dir().ok()?;
    dir.is_dir().then_some(dir)
}

/// The cross-agent skills tree (`~/.agents/skills`), when it exists on
/// disk — the read-only `agents` mount's source, the same 1:1 posture
/// the rhema skills bind takes.
pub(crate) fn agents_skills_dir() -> Option<PathBuf> {
    let dir = rhema_config::Config::agents_skills_dir().ok()?;
    dir.is_dir().then_some(dir)
}

/// Seeds a fresh sandbox workspace once: the skills (no git seed —
/// rhema-code has no repository to clone), then the user's SYSTEM.md
/// when one exists on disk, each written through the sandbox the way
/// the cloud frontends seed. The marker is written last, after a
/// successful seed, and checked first: the sandbox persists across
/// turns, and a re-seed would clone over live work.
pub(crate) async fn ensure_seeded(
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

/// The same discovery the local path's prompt renders — the
/// cross-agent `~/.agents/skills` under rhema's global config
/// directory under the workspace's `.rhema/skills`, a higher tier
/// shadowing a lower by name — paired with each body read host-side
/// now. The seeded postures' returned list names the rewritten,
/// sandbox-side paths and carries the bodies `seed_workspace` writes; a
/// body that cannot be read here would be unreadable there too, so it
/// drops from both. Mounted, paths stand as discovered — the
/// workspace's and the two global trees' 1:1 mounts put them at their
/// own paths inside the instance, the mounts are the delivery, nothing
/// is copied, and a skill edited mid-session is simply the new text
/// the next read sees. Only a skill outside all three trees rides the
/// seed, which on the mountless postures is the whole list anyway. The
/// two global directories arrive resolved — `Config::agents_skills_dir`
/// and `Config::skills_dir` at the call site — so a test points them
/// at scratch paths instead of the machine's own.
pub(crate) fn discovered_skills(
    agents: Option<&Path>,
    global: Option<&Path>,
    workspace: &Path,
    mounted: bool,
    sandbox_workspace: &str,
) -> (Vec<Skill>, Vec<(String, String)>) {
    let mut skills = Vec::new();
    let mut bodies = Vec::new();
    let discovered = rhema_agent_tools::skills::discover(agents, global, workspace);
    for skill in discovered {
        // A mounted tree's skill needs no delivery; anything outside
        // the three trees travels as a body into the workspace seed.
        let on_a_mount = skill.path.starts_with(workspace)
            || global.is_some_and(|dir| skill.path.starts_with(dir))
            || agents.is_some_and(|dir| skill.path.starts_with(dir));
        if mounted && on_a_mount {
            skills.push(skill);
            continue;
        }
        bodies.push((skill.name.clone(), skill.body.clone()));
        skills.push(Skill {
            name: skill.name.clone(),
            description: skill.description,
            license: skill.license,
            path: PathBuf::from(format!(
                "{sandbox_workspace}/.rhema/skills/{}/SKILL.md",
                skill.name
            )),
            body: skill.body,
        });
    }
    (skills, bodies)
}
