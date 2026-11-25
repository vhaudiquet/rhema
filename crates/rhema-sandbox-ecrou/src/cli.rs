//! The ecrou CLI from this side: the payload line's shell, the
//! scratch both sides see, and the argv one boot crosses with.

use std::path::Path;

use crate::gitenv::GitEnv;
use rhema_sandbox::SandboxError;
use rhema_sandbox::exec_util;

/// The payload line: the command through `bash -c` when the image
/// carries bash, through its `sh` when it does not, the seam says
/// `bash -c`, and an alpine rootfs ships no bash. `exec` hands the
/// streams and the exit code straight through either way.
pub(crate) fn shell_line(command: &str) -> String {
    let quoted = exec_util::shquote(command);
    format!(
        "if command -v bash >/dev/null 2>&1; then \
         exec bash -c {quoted}; else exec sh -c {quoted}; fi"
    )
}

/// The capture scratch, in a directory both sides see. A native ecrou
/// shares `/tmp` with this process, so the default tempdir is right;
/// the snap runs in a private `/tmp` of its own, where a shared
/// directory would take the payload's streams and vanish, the
/// machine writes them, the host never sees a byte. Under the snap
/// the scratch moves to the snap's durable area
/// (`~/snap/ecrou/common`), which the confinement maps to
/// `$SNAP_USER_COMMON` and the home interface grants; the tempdir's
/// drop cleans it either way.
pub(crate) fn capture_dir() -> Result<tempfile::TempDir, SandboxError> {
    scratch_dir().map_err(|err| SandboxError::Unavailable(format!("the capture directory: {err}")))
}

/// A scratch directory the `ecrou` on PATH can see, for anything that
/// crosses the process boundary, the capture files, and the tests'
/// workspace shares. The snap's `/tmp` is a private per-run tmpfs: a
/// host-created directory there is invisible to the machine, and
/// ecrou refuses the share ("share host path does not exist") or,
/// when the capture path happens to sit inside the share, silently
/// materializes an empty copy of it and shares that, output
/// swallowed, exit green. The snap's durable area
/// (`~/snap/ecrou/common`) is on the real filesystem and visible to
/// both sides, so scratch under the snap roots there; a native build
/// shares `/tmp` with this process and keeps the default. The plain
/// prefix is for the snap's other rule: dot entries under `$HOME` are
/// silently invisible to it, and tempfile's default `.tmp` prefix is
/// exactly that.
pub(crate) fn scratch_dir() -> std::io::Result<tempfile::TempDir> {
    let lookup = snap_lookup_path().and_then(|p| std::fs::canonicalize(p).ok());
    match snap_common(lookup.as_deref()) {
        Some(root) => {
            std::fs::create_dir_all(&root)?;
            tempfile::Builder::new()
                .prefix("rhema-ecrou-")
                .rand_bytes(6)
                .tempdir_in(root)
        }
        None => tempfile::Builder::new()
            .prefix("rhema-ecrou-")
            .rand_bytes(6)
            .tempdir(),
    }
}

/// The runtime root ecrou can actually see: the snap's private `/tmp`
/// hides the host's, so a snap install keeps its runtime under the
/// snap's durable area; a native build uses the default.
pub fn shared_root(default: &Path) -> std::path::PathBuf {
    let lookup = snap_lookup_path().and_then(|p| std::fs::canonicalize(p).ok());
    match snap_common(lookup.as_deref()) {
        Some(root) => root.join("rhema-runtime"),
        None => default.to_path_buf(),
    }
}

/// Resolve the `ecrou` name the way exec will, so the capture scratch
/// lands where the binary that writes it can reach. The backend
/// spawns the name bare, so the search is PATH's own.
fn snap_lookup_path() -> Option<std::path::PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join("ecrou"))
        .find(|candidate| candidate.is_file())
}

/// The snap's durable area when the resolved binary is snapd's
/// launcher: `/snap/bin/ecrou` is a symlink into snapd's own tree,
/// and a native build never is.
pub(crate) fn snap_common(resolved: Option<&std::path::Path>) -> Option<std::path::PathBuf> {
    let resolved = resolved?;
    let confined = resolved == std::path::Path::new("/usr/bin/snap")
        || resolved.starts_with("/usr/lib/snapd/")
        || resolved.starts_with("/snap/");
    if !confined {
        return None;
    }
    let home = std::env::var_os("HOME")?;
    Some(
        std::path::Path::new(&home)
            .join("snap")
            .join("ecrou")
            .join("common"),
    )
}

/// The run argv: options first, the image, then the payload after
/// `--`. Nothing in the payload is quoted for a middle shell, the
/// argv crosses the CLI as argv, and the init script's own quoting
/// takes it from there.
#[allow(clippy::too_many_arguments)]
pub(crate) fn run_argv(
    bin: &str,
    image: &str,
    capture: &Path,
    shares: &[&str],
    workdir: &str,
    command: &str,
    git_env: Option<&GitEnv>,
    persist: Option<&str>,
    persist_reset: bool,
) -> Vec<String> {
    let mut argv = vec![
        bin.to_owned(),
        "run".to_owned(),
        // The console is diagnostics; the payload's streams cross the
        // capture share. Quiet also keeps the progress renderer off
        // stderr, which this process reads.
        "--quiet".to_owned(),
        "--capture".to_owned(),
        capture.to_string_lossy().into_owned(),
    ];
    for share in shares {
        argv.push("-v".to_owned());
        argv.push((*share).to_owned());
    }
    argv.extend(["--workdir".to_owned(), workdir.to_owned()]);
    if let Some(env) = git_env {
        argv.push("--env".to_owned());
        argv.push(format!("GIT_CONFIG_GLOBAL={}", env.guest_config_path));
    }
    if let Some(key) = persist {
        if persist_reset {
            argv.push("--persist-reset".to_owned());
        } else {
            argv.push("--persist".to_owned());
        }
        argv.push(key.to_owned());
    }
    argv.push(image.to_owned());
    argv.push("--".to_owned());
    argv.push("/bin/sh".to_owned());
    argv.push("-c".to_owned());
    argv.push(shell_line(command));
    argv
}
