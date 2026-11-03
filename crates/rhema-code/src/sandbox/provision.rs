//! The per-backend instance preparation: the idempotent provision script,
//! the ensure paths, the mounts and devices, and the argv each backend
//! execs.

use super::*;

/// The preparation script: marker-gated and idempotent. The marker
/// records the list it installed, so a list change re-runs the
/// install; a failed install writes no marker, so the next assembly
/// tries again. Runs as the instance's root — apt is system work,
/// and nothing here touches the mounted tree.
pub(crate) fn provision_script() -> String {
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
pub(crate) fn provision_attempted(backend: &str, name: &str) -> bool {
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
pub(crate) fn lxd_provision_argv(name: &str) -> Vec<String> {
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

pub(crate) fn docker_provision_argv(name: &str) -> Vec<String> {
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
/// workspace at its own path, held open between runs, plus the two
/// global skills trees read-only when they exist. Posture, plainly:
/// process hygiene, not a security boundary — the container's commands
/// reach the same fs the local path would.
///
/// The CLI's waits are blocking, so the work hops to the blocking
/// pool; the assembly's scheduler keeps its worker.
pub(crate) async fn ensure_container(
    name: &str,
    workspace: &str,
    skills: Option<&std::path::Path>,
    agents: Option<&std::path::Path>,
) -> anyhow::Result<()> {
    let name = name.to_owned();
    let workspace = workspace.to_owned();
    let skills = skills.map(std::path::PathBuf::from);
    let agents = agents.map(std::path::PathBuf::from);
    tokio::task::spawn_blocking(move || {
        ensure_container_blocking(&name, &workspace, skills.as_deref(), agents.as_deref())
    })
    .await
    .map_err(|err| anyhow::anyhow!("the docker ensure task failed: {err}"))?
}

fn ensure_container_blocking(
    name: &str,
    workspace: &str,
    skills: Option<&std::path::Path>,
    agents: Option<&std::path::Path>,
) -> anyhow::Result<()> {
    if let Some(mounts) = docker_mounts(name)? {
        if mounts_current(&mounts, workspace, skills, agents) {
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
    let argv = docker_run_argv(name, workspace, skills, agents);
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
pub(crate) fn docker_mounts(name: &str) -> anyhow::Result<Option<String>> {
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
/// workspace bind, and — when each global skills directory exists —
/// its read-only bind. Matched by the inspect JSON's `"Source"`
/// entries — the host side, the same string the run argv names — so a
/// workspace that is a prefix of some other mount's path cannot pass
/// for its own bind. A path the JSON's escaping mangles reads as
/// missing: the container is recreated with the correct mounts, the
/// slow path, never the wrong one.
pub(crate) fn mounts_current(
    mounts_json: &str,
    workspace: &str,
    skills: Option<&std::path::Path>,
    agents: Option<&std::path::Path>,
) -> bool {
    if !mounts_json.contains(&source_entry(workspace)) {
        return false;
    }
    [skills, agents]
        .into_iter()
        .flatten()
        .all(|dir| mounts_json.contains(&source_entry(&dir.to_string_lossy())))
}

/// The inspect JSON's source entry for one host path:
/// `"Source":"<path>"`.
pub(crate) fn source_entry(path: &str) -> String {
    format!(r#""Source":"{path}""#)
}

/// The run argv: `docker run -d --name <name> -v <ws>:<ws>
/// [-v <skills>:<skills>:ro] [-v <agents>:<agents>:ro]
/// debian:trixie sleep infinity` — the workspace bind is the
/// container's filesystem story, the two global skills binds ride
/// read-only when their directories exist.
pub(crate) fn docker_run_argv(
    name: &str,
    workspace: &str,
    skills: Option<&std::path::Path>,
    agents: Option<&std::path::Path>,
) -> Vec<String> {
    let mut argv = vec![
        "run".to_owned(),
        "-d".to_owned(),
        "--name".to_owned(),
        name.to_owned(),
        "-v".to_owned(),
        bind_mount(workspace, false),
    ];
    for dir in [skills, agents].into_iter().flatten() {
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
pub(crate) fn bind_mount(source: &str, readonly: bool) -> String {
    if readonly {
        format!("{source}:{source}:ro")
    } else {
        format!("{source}:{source}")
    }
}

/// Prepares an instance once per process: the marker gate inside the
/// script makes the remote side idempotent, and this gate keeps the
/// check itself from running a turn after the first.
pub(crate) fn prepare_instance(backend: &str, name: &str, argv: &[String]) {
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
pub(crate) fn container_running(name: &str) -> anyhow::Result<bool> {
    let output = std::process::Command::new("docker")
        .args(["inspect", "-f", "{{.State.Running}}", name])
        .output()
        .map_err(|err| anyhow::anyhow!("docker inspect failed: {err}"))?;
    Ok(output.status.success() && String::from_utf8_lossy(&output.stdout).trim() == "true")
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
pub(crate) fn wanted_idmap(uid: u32, gid: u32) -> String {
    format!("both {uid} {gid}")
}

/// The launch argv: `launch [--vm] ubuntu:24.04 <name> -c
/// raw.idmap=both <uid> <gid>` — the map set at birth, when it
/// applies on the first boot for free.
pub(crate) fn lxd_launch_argv(name: &str, vm: bool, uid: u32, gid: u32) -> Vec<String> {
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
pub(crate) fn workspace_owner(workspace: &std::path::Path) -> anyhow::Result<(u32, u32)> {
    use std::os::unix::fs::MetadataExt;
    let meta = std::fs::metadata(workspace)
        .map_err(|err| anyhow::anyhow!("the lxd sandbox needs the workspace's owner: {err}"))?;
    Ok((meta.uid(), meta.gid()))
}

pub(crate) fn lxc_output(args: &[&str]) -> anyhow::Result<std::process::Output> {
    std::process::Command::new("lxc")
        .args(args)
        .output()
        .map_err(|err| anyhow::anyhow!("lxc {} failed: {err}", args.join(" ")))
}

/// The lxc CLI's waits are blocking — `launch` can take minutes — so
/// the work hops to the blocking pool; the assembly's scheduler keeps
/// its worker.
pub(crate) async fn ensure_lxd_instance(
    name: &str,
    vm: bool,
    uid: u32,
    gid: u32,
) -> anyhow::Result<()> {
    let name = name.to_owned();
    tokio::task::spawn_blocking(move || ensure_lxd_instance_blocking(&name, vm, uid, gid))
        .await
        .map_err(|err| anyhow::anyhow!("the lxd ensure task failed: {err}"))?
}

fn ensure_lxd_instance_blocking(name: &str, vm: bool, uid: u32, gid: u32) -> anyhow::Result<()> {
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
pub(crate) fn idmap_of(name: &str) -> anyhow::Result<String> {
    let output = lxc_output(&["config", "get", name, "raw.idmap"])?;
    if !output.status.success() {
        anyhow::bail!(
            "lxc config get failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

/// Adds the workspace's `disk` device — and each global skills
/// directory's, read-only, when it exists: `lxc config device add
/// <name> <device> disk source=<dir> path=<dir>` — the 1:1 mirrors
/// that make the sandbox-side paths the host paths themselves.
/// Containers only; a virtual machine takes no host-path mount and
/// rides the attached posture instead.
/// The device attach is an lxc CLI wait — blocking, so it hops to the
/// blocking pool like the rest of the ensure path.
pub(crate) async fn ensure_lxd_mount(
    name: &str,
    workspace: &str,
    skills: Option<&std::path::Path>,
    agents: Option<&std::path::Path>,
) -> anyhow::Result<()> {
    let name = name.to_owned();
    let workspace = workspace.to_owned();
    let skills = skills.map(std::path::PathBuf::from);
    let agents = agents.map(std::path::PathBuf::from);
    tokio::task::spawn_blocking(move || {
        ensure_lxd_mount_blocking(&name, &workspace, skills.as_deref(), agents.as_deref())
    })
    .await
    .map_err(|err| anyhow::anyhow!("the lxd mount task failed: {err}"))?
}

fn ensure_lxd_mount_blocking(
    name: &str,
    workspace: &str,
    skills: Option<&std::path::Path>,
    agents: Option<&std::path::Path>,
) -> anyhow::Result<()> {
    let devices = lxd_devices(name)?;
    lxd_device(name, &devices, "workspace", workspace, false)?;
    let binds = [("skills", skills), ("agents", agents)];
    for (device, dir) in binds
        .into_iter()
        .filter_map(|(d, dir)| dir.map(|dir| (d, dir)))
    {
        // Re-shown: the first add changed the set this check reads.
        let devices = lxd_devices(name)?;
        let dir = dir.to_string_lossy().into_owned();
        lxd_device(name, &devices, device, &dir, true)?;
    }
    Ok(())
}

/// The instance's device set as `lxc config device show` prints it.
pub(crate) fn lxd_devices(name: &str) -> anyhow::Result<String> {
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
pub(crate) fn lxd_device(
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
pub(crate) fn lxd_device_argv(name: &str, device: &str, dir: &str, readonly: bool) -> Vec<String> {
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

#[cfg(test)]
mod provision_argv {
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
