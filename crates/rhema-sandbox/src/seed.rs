//! The shared half of the two `ensure_seeded`s.
//!
//! `ensure_seeded` exists twice — a DO meta table and a Postgres marker
//! around the same logic: check the marker, git-clone the seed, write
//! the skills (spec §1). [`seed_workspace`] is the logic, shared; the
//! marker stays with the caller, because it lives in each platform's
//! store.
#![forbid(unsafe_code)]

use crate::{Sandbox, SandboxError, WorkspaceSeed};

/// Seeds a fresh workspace: the git clone when the session has one,
/// then every skill body at `{workspace}/.rhema/skills/{name}/SKILL.md`
/// — the paths the prompt's skill list points the model at. Whether the
/// workspace needed seeding at all is the caller's marker to check.
pub async fn seed_workspace(
    sandbox: &dyn Sandbox,
    seed: Option<&str>,
    workspace: &str,
    skills: &[(&str, &str)],
) -> Result<(), SandboxError> {
    if let Some(url) = seed {
        sandbox
            .seed(&WorkspaceSeed::GitClone(url.to_owned()))
            .await?;
    }
    let workspace = std::path::Path::new(workspace);
    for (name, body) in skills {
        let path = rhema_core::workspace::skills_dir(workspace)
            .join(name)
            .join("SKILL.md");
        let path = path.display().to_string();
        sandbox
            .write_file(&path, body.as_bytes())
            .await
            .map_err(|err| SandboxError::Failed(format!("seeding skill {name}: {err}")))?;
    }
    Ok(())
}

#[cfg(all(test, feature = "mock"))]
mod tests {
    use super::*;
    use crate::mock::ScriptedSandbox;

    #[tokio::test]
    async fn it_clones_the_seed_and_writes_each_skill_body() {
        let sandbox = ScriptedSandbox::new();
        seed_workspace(
            &sandbox,
            Some("https://example.com/repo"),
            "/workspace",
            &[("review", "# review"), ("deploy", "# deploy")],
        )
        .await
        .expect("the seeding lands");
        assert_eq!(
            sandbox.seeds(),
            vec![WorkspaceSeed::GitClone("https://example.com/repo".into())]
        );
        let files = sandbox.files();
        assert_eq!(
            files
                .get("/workspace/.rhema/skills/review/SKILL.md")
                .map(Vec::as_slice),
            Some(b"# review".as_slice())
        );
        assert_eq!(
            files
                .get("/workspace/.rhema/skills/deploy/SKILL.md")
                .map(Vec::as_slice),
            Some(b"# deploy".as_slice())
        );
    }

    #[tokio::test]
    async fn a_session_without_a_seed_makes_no_git_call() {
        let sandbox = ScriptedSandbox::new();
        seed_workspace(&sandbox, None, "/workspace", &[])
            .await
            .expect("the empty seeding lands");
        assert!(sandbox.seeds().is_empty());
        assert!(sandbox.files().is_empty());
    }
}
