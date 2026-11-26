//! The sandbox module's tests: choice resolution, the assembly's
//! tool order, the seed decision, and the per-backend argv.

use super::machines::SandboxAvailability;
use super::provision::{docker_run_argv, lxd_device_argv, mounts_current};
use super::seed::{discovered_skills, ensure_seeded};
use super::*;
use crate::args::SandboxChoice;
use rhema_sandbox::mock::ScriptedSandbox;

/// A backend the config needs to exist; these tests never run a turn.
struct Unused;

#[rhema_core::async_trait]
impl rhema_core::ModelProvider for Unused {
    async fn complete(
        &self,
        _req: rhema_core::ModelRequest,
        _sink: &mut dyn rhema_core::EventSink,
    ) -> Result<rhema_core::ModelOutput, rhema_core::ProviderError> {
        Err(rhema_core::ProviderError::Other("unused provider".into()))
    }
}

#[test]
fn the_choice_resolves_from_the_layered_kind() {
    // The env tier is injected as the value Profile::sandbox_kind
    // would return with RHEMA_SANDBOX set: it wins over the stored
    // kind.
    assert_eq!(choice_from_kind(Some("docker")), SandboxChoice::Docker);
    assert_eq!(choice_from_kind(Some("http")), SandboxChoice::Http);
    // An explicit local, nothing set, and an unparseable spelling
    // all read as the fast path, the zero-flag invocation never
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
    let (skills, bodies) = discovered_skills(None, None, &ws, true, &ws.to_string_lossy());
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
    let (skills, bodies) = discovered_skills(None, None, &ws, false, "/workspace");
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

/// One skill in a cross-agent `.agents` tree, the home's or the
/// workspace's, for the discovery tests: the home tree no posture
/// mounts.
fn agents_with_skill(root: &Path) {
    let skill = root.join(".agents").join("skills").join("borrowed");
    std::fs::create_dir_all(&skill).expect("the skill directory");
    std::fs::write(
        skill.join("SKILL.md"),
        "---\ndescription: a borrowed trigger\n---\n# borrowed\n",
    )
    .expect("the skill lands");
}

#[test]
fn a_mounted_agents_skill_stands_at_its_own_path_and_seeds_nothing() {
    let dir = workspace_with_skill();
    let ws = dir.path().to_path_buf();
    let agents = tempfile::tempdir().expect("tempdir");
    agents_with_skill(agents.path());
    let (skills, bodies) = discovered_skills(
        Some(&agents.path().join(".agents").join("skills")),
        None,
        &ws,
        true,
        &ws.to_string_lossy(),
    );
    let skill = skills
        .iter()
        .find(|skill| skill.name == "borrowed")
        .expect("the .agents skill is discovered");
    assert_eq!(
        skill.path,
        agents
            .path()
            .join(".agents")
            .join("skills")
            .join("borrowed")
            .join("SKILL.md"),
        "the read-only agents mount puts the tree at its own path"
    );
    assert!(bodies.is_empty(), "the mounts are the delivery: {bodies:?}");
}

#[test]
fn an_agents_skill_rides_the_seed_when_the_posture_mounts_nothing() {
    let dir = workspace_with_skill();
    let ws = dir.path().to_path_buf();
    let agents = tempfile::tempdir().expect("tempdir");
    agents_with_skill(agents.path());
    let (skills, bodies) = discovered_skills(
        Some(&agents.path().join(".agents").join("skills")),
        None,
        &ws,
        false,
        "/workspace",
    );
    let skill = skills
        .iter()
        .find(|skill| skill.name == "borrowed")
        .expect("the .agents skill is discovered");
    assert_eq!(
        skill.path,
        PathBuf::from("/workspace/.rhema/skills/borrowed/SKILL.md"),
        "a mountless posture cannot read the host tree: the seed delivers it"
    );
    assert!(
        bodies
            .iter()
            .any(|(name, body)| name == "borrowed" && body.contains("# borrowed")),
        "the body rides the seed: {bodies:?}"
    );
}

#[test]
fn a_mounted_workspace_agents_skill_stands_at_its_own_path_and_seeds_nothing() {
    let dir = workspace_with_skill();
    let ws = dir.path().to_path_buf();
    agents_with_skill(&ws);
    let (skills, bodies) = discovered_skills(None, None, &ws, true, &ws.to_string_lossy());
    let skill = skills
        .iter()
        .find(|skill| skill.name == "borrowed")
        .expect("the workspace .agents skill is discovered");
    assert_eq!(
        skill.path,
        ws.join(".agents")
            .join("skills")
            .join("borrowed")
            .join("SKILL.md"),
        "the workspace mount puts the tree at its own path"
    );
    assert!(bodies.is_empty(), "the mounts are the delivery: {bodies:?}");
}

#[test]
fn a_workspace_agents_skill_rides_the_seed_when_the_posture_mounts_nothing() {
    let dir = workspace_with_skill();
    let ws = dir.path().to_path_buf();
    agents_with_skill(&ws);
    let (skills, bodies) = discovered_skills(None, None, &ws, false, "/workspace");
    let skill = skills
        .iter()
        .find(|skill| skill.name == "borrowed")
        .expect("the workspace .agents skill is discovered");
    assert_eq!(
        skill.path,
        PathBuf::from("/workspace/.rhema/skills/borrowed/SKILL.md"),
        "a mountless posture cannot read the host tree: the seed delivers it"
    );
    assert!(
        bodies
            .iter()
            .any(|(name, body)| name == "borrowed" && body.contains("# borrowed")),
        "the body rides the seed: {bodies:?}"
    );
}

#[test]
fn the_run_argv_binds_the_workspace_and_the_read_only_skills() {
    assert_eq!(
        docker_run_argv("box", "/home/me/proj", None, None, None),
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
        docker_run_argv("box", "/home/me/proj", None, Some(skills), None),
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
    let agents = Path::new("/h/.agents/skills");
    assert_eq!(
        docker_run_argv("box", "/home/me/proj", None, Some(skills), Some(agents)),
        vec![
            "run",
            "-d",
            "--name",
            "box",
            "-v",
            "/home/me/proj:/home/me/proj",
            "-v",
            "/h/.config/rhema/skills:/h/.config/rhema/skills:ro",
            "-v",
            "/h/.agents/skills:/h/.agents/skills:ro",
            "debian:trixie",
            "sleep",
            "infinity",
        ]
        .into_iter()
        .map(String::from)
        .collect::<Vec<_>>()
    );
    let runtime = Path::new("/run/user/1000/rhema");
    assert_eq!(
        docker_run_argv("box", "/home/me/proj", Some(runtime), None, None),
        vec![
            "run",
            "-d",
            "--name",
            "box",
            "-v",
            "/home/me/proj:/home/me/proj",
            "-v",
            "/run/user/1000/rhema:/run/user/1000/rhema",
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
    // see, clean, failed, never started, each have a shape.
    assert_eq!(stop_instance("true", &[]).ok(), Some(true));
    assert_eq!(stop_instance("false", &[]).ok(), Some(false));
    assert!(stop_instance("rhema-no-such-program", &[]).is_err());
}

#[test]
fn a_mount_set_missing_the_skills_bind_is_stale() {
    let mounts = r#"[{"Type":"bind","Source":"/home/me/proj","Destination":"/home/me/proj"}]"#;
    let skills = Some(Path::new("/h/.config/rhema/skills"));
    let agents = Some(Path::new("/h/.agents/skills"));
    assert!(mounts_current(mounts, "/home/me/proj", None, None, None));
    assert!(!mounts_current(mounts, "/home/me/proj", None, skills, None));
    let whole = r#"[{"Source":"/home/me/proj"},{"Source":"/h/.config/rhema/skills"}]"#;
    assert!(mounts_current(whole, "/home/me/proj", None, skills, None));
    // Both global binds must be present: a container from before
    // the agents mount is stale, whatever else it carries.
    assert!(!mounts_current(
        whole,
        "/home/me/proj",
        None,
        skills,
        agents
    ));
    let everything = r#"[{"Source":"/home/me/proj"},{"Source":"/h/.config/rhema/skills"},{"Source":"/h/.agents/skills"}]"#;
    assert!(mounts_current(
        everything,
        "/home/me/proj",
        None,
        skills,
        agents
    ));
    // A workspace that is a prefix of another mount's path does not
    // pass for its own bind.
    assert!(!mounts_current(
        r#"[{"Source":"/home/me/proj2"}]"#,
        "/home/me/proj",
        None,
        None,
        None
    ));
    // The runtime mount is part of the set: a container without it
    // is stale even when the workspace and skills binds are right.
    let runtime = Path::new("/run/user/1000/rhema");
    assert!(!mounts_current(
        whole,
        "/home/me/proj",
        Some(runtime),
        skills,
        None
    ));
    let shared = r#"[{"Source":"/home/me/proj"},{"Source":"/h/.config/rhema/skills"},{"Source":"/run/user/1000/rhema"}]"#;
    assert!(mounts_current(
        shared,
        "/home/me/proj",
        Some(runtime),
        skills,
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
fn the_http_session_derives_the_container_name() {
    // The http session is the container derivation: bare basenames
    // would collide on one daemon (two `repo` workspaces sharing a
    // root directory).
    let name = derived_container_name(Path::new("/home/me/proj"));
    assert!(name.starts_with("rhema-sandbox-proj-"), "{name}");
    assert_ne!(name, derived_container_name(Path::new("/home/me/other")));
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
        SandboxAvailability::from_parts(true, false, false, true, &host).offers(SandboxChoice::Ssh)
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
        target: "http://box:7331".to_owned(),
        tools: Arc::new(ShellTools::new("/workspace")),
        workspace: "/workspace".to_owned(),
        skills: vec![Skill {
            name: "review".into(),
            description: "trigger".into(),
            license: None,
            path: PathBuf::from("/workspace/.rhema/skills/review/SKILL.md"),
            tier: rhema_prompt::SkillTier::Workspace,
            body: "---\ndescription: trigger\n---\nthe review rules".into(),
        }],
        instance_lock: None,
        managed: None,
        shared: None,
        mcp: crate::mcp_config::McpStore::local(None, &std::collections::BTreeSet::new()),
        session: std::sync::Mutex::new(None),
        sandbox: None,
        remote: None,
        remote_session: Default::default(),
    };
    let mut config = HostAgentConfig::new(
        Arc::new(Unused),
        "m",
        Path::new("/host/ws"),
        rhema_host_agent::AllowAll,
    );
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

#[test]
fn a_remote_kit_env_names_the_helper_and_the_session_state() {
    let remote = rhema_sandbox::kit::SandboxKit {
        helper: "/home/u/.cache/rhema/kit/3/rhema".into(),
        helper_dir: "/home/u/.cache/rhema/kit/3".into(),
        init: "/home/u/.cache/rhema/runtime/init.sh".into(),
        root: "/home/u/.cache/rhema/runtime".into(),
        gob: true,
    };
    let stub = |name: &str, bin: &str, mode: &str| {
        rhema_bash::runtime::Stub::new(name, bin, vec![mode.to_owned()]).unwrap()
    };
    let mut jobs = rhema_bash::runtime::Kit::new(
        rhema_bash::runtime::Feature::Jobs,
        vec![stub("job", "RHEMA_JOBS_BIN", "__jobs")],
        rhema_bash::runtime::Needs::MACRO,
    );
    jobs.vars
        .insert("RHEMA_JOBS_BIN".into(), "/host/bin/rhema".into());
    jobs.vars.insert("RHEMA_SESSION".into(), "tag".into());
    let vision = rhema_bash::runtime::Kit::new(
        rhema_bash::runtime::Feature::Vision,
        Vec::new(),
        rhema_bash::runtime::Needs::SHELL_STATE,
    )
    .with_shell(rhema_bash::runtime::Shell::new("see", "printf '%s\\n' staged").unwrap());
    let env = remote_kit_env(&remote, "k1", true, true, &[jobs, vision]);
    assert_eq!(
        env.get("BASH_ENV").map(String::as_str),
        Some("/home/u/.cache/rhema/runtime/init.sh")
    );
    assert_eq!(
        env.get("RHEMA_JOBS_BIN").map(String::as_str),
        Some("/home/u/.cache/rhema/kit/3/rhema")
    );
    assert!(
        !env.contains_key("RHEMA_VISION_BIN"),
        "the vision macro is shell, not a helper call"
    );
    assert_eq!(
        env.get("RHEMA_JOBS_SESSION").map(String::as_str),
        Some("/home/u/.cache/rhema/runtime/sessions/k1/jobs/session")
    );
    assert_eq!(
        env.get("RHEMA_VISION_STAGING").map(String::as_str),
        Some("/home/u/.cache/rhema/runtime/sessions/k1/vision/staging")
    );
    assert_eq!(env.get("RHEMA_SESSION").map(String::as_str), Some("tag"));
}

#[tokio::test]
async fn a_remote_vision_pull_reads_the_staged_file_and_consumes_it() {
    let sandbox = Arc::new(ScriptedSandbox::new());
    let image = b"\x89PNG\r\n\x1a\nrest".to_vec();
    sandbox
        .write_file(
            "/home/u/.cache/rhema/runtime/sessions/k/vision/staging/1.img",
            &image,
        )
        .await
        .unwrap();
    sandbox.push_run(Ok(rhema_sandbox::RunOutcome {
        stdout: b"/home/u/.cache/rhema/runtime/sessions/k/vision/staging/1.img\n".to_vec(),
        stderr: Vec::new(),
        code: Some(0),
    }));
    let pull = SandboxVisionPull {
        sandbox: sandbox.clone(),
        workspace: "/workspace".to_owned(),
        staging: "/home/u/.cache/rhema/runtime/sessions/k/vision/staging".to_owned(),
    };
    let files = rhema_bash::vision::VisionPull::take(&pull).await;
    assert_eq!(files, vec![image]);
    let executed = sandbox.executed();
    assert!(executed[0].starts_with("find "), "{executed:?}");
    assert!(executed[1].starts_with("rm -f "), "{executed:?}");
}

#[test]
fn the_static_sandbox_job_kit_drives_gob_from_shell() {
    let kits = sandbox_stub_kits();
    let jobs = kits
        .iter()
        .find(|kit| kit.feature == rhema_bash::runtime::Feature::Jobs)
        .expect("the jobs kit");
    assert!(
        jobs.stubs.is_empty(),
        "a sandbox job macro must not exec a helper"
    );
    assert_eq!(jobs.shells.len(), 1);
    assert_eq!(jobs.shells[0].name, "job");
    assert!(
        jobs.shells[0].body.contains("gob list --json"),
        "the macro drives gob directly"
    );
}
