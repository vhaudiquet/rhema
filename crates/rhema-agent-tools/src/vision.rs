//! The `see` macro: how a command's output becomes an image the model
//! sees.
//!
//! There is no native tool for this — images are shell data like any
//! other, so the surface is a bash function the model pipes into:
//!
//! ```ignore
//! curl -sL https://example.com/photo.png | see
//! see screenshots/capture.png
//! ```
//!
//! Two halves, joined by a staging directory:
//!
//! - [`provision_with_bin`] writes the init file one non-interactive
//!   bash sources from `BASH_ENV` (the same fact the MCP provision
//!   rests on, `specs/rhema-mcp.md` §2): `see()` execs the rhema
//!   binary in its `__vision` mode — [`cli_run`] — which sniffs the
//!   bytes, stages them, and says so on stdout. The wiring's env map
//!   carries the init file's path, the binary's, and the staging
//!   directory's; [`wire`] builds it once per process and a frontend
//!   layers the map over `CodingAgentConfig::shell_env`.
//! - [`attach`] runs after every `bash` command that reaches an exit:
//!   [`ShellTools`](crate::ShellTools) drains the staging directory,
//!   turns each staged image into a [`ToolResult`] image block, and
//!   notes the rest in text. Bytes never ride stdout, so output caps
//!   and truncation cannot eat them.
//!
//! A staged image a command never got to see — the model staged it, it
//! belongs in the model's context — attaches even when the command
//! failed or timed out. A session whose bash runs elsewhere (a remote
//! sandbox, an executor behind a wire) wires nothing: its commands
//! cannot write where [`attach`] reads, the same rule the MCP wire
//! applies for its socket.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use base64::Engine as _;
use rhema_core::{Image, ImageMediaType, ToolResult};

/// The bash function's name, and the model's whole interface. A rename
/// is this constant plus the prompt section and the spec text.
pub const NAME: &str = "see";

/// The hidden binary mode the init file's function execs: `rhema
/// __vision …`. The public interface is the function; the mode is a
/// transport detail, like the MCP CLI's `__server`.
pub const MODE: &str = "__vision";

/// The env var non-interactive bash reads its init file from — the
/// same var the MCP provision uses, so whichever init file the wiring
/// names last, its baked predecessor line reaches the other.
pub const VAR_BASH_ENV: &str = "BASH_ENV";
/// The env var holding the binary the function execs.
pub const VAR_BIN: &str = "RHEMA_VISION_BIN";
/// The env var holding the staging directory the function writes and
/// [`attach`] drains.
pub const VAR_STAGING: &str = "RHEMA_VISION_STAGING";

/// The largest image one staged file may carry: 5 MB, the API-side
/// per-image ceiling of the Anthropic Messages API — the strictest
/// target. Larger images are refused at both ends of the pipe, so the
/// model learns the limit from the error and not from a silent drop.
pub const MAX_IMAGE_BYTES: usize = 5 * 1024 * 1024;

/// The most images one tool result may carry. A command can stage
/// dozens of screenshots; past this cap the extras stay dropped, with
/// the count in the result's text, because a result of unbounded image
/// blocks is a context-budget accident waiting for a `for` loop.
const MAX_IMAGES_PER_RESULT: usize = 8;

/// The system-prompt section a wired frontend renders: the macro's
/// whole documentation. Kept beside the code so prompt and behavior
/// cannot drift.
pub const PROMPT_SECTION: &str = "You can view images by piping bytes into `see`, or by passing file paths:

    curl -sL https://example.com/photo.png | see
    see screenshots/capture.png

Staged images attach to that bash call's result and you see them as images. Formats: png, jpeg, gif, webp; up to 5 MB per image.";

/// Why bytes did not stage. The CLI turns every variant into a stderr
/// line and an exit code, so the model reads the reason in-band.
#[derive(Debug, thiserror::Error)]
pub enum StageError {
    /// Nothing arrived on stdin.
    #[error("no bytes arrived on stdin")]
    Empty,
    /// The magic bytes name no format the providers accept.
    #[error("not a recognized image (png, jpeg, gif, webp)")]
    NotAnImage,
    /// The bytes pass [`MAX_IMAGE_BYTES`].
    #[error("image too large: {0} bytes, the limit is {MAX_IMAGE_BYTES}")]
    TooLarge(usize),
    /// The staging directory is unwritable or was never wired.
    #[error("staging failed: {0}")]
    Io(#[from] std::io::Error),
}

/// One process's wiring: the env map a frontend layers over
/// `CodingAgentConfig::shell_env`, the prompt section to render, and
/// the provisioned directory held for the process's life — dropping it
/// deletes the init file and any staged files under it, so the wiring
/// must outlive every session it feeds.
pub struct Wiring {
    /// The env map: `BASH_ENV`, `RHEMA_VISION_BIN`, and
    /// `RHEMA_VISION_STAGING`. Deliberately no inherited-`BASH_ENV`
    /// entry: the chain is baked into the init file's text, and an env
    /// var here would make an outer generated init re-source itself
    /// until the shell died.
    pub shell_env: BTreeMap<String, String>,
    /// The prompt's `vision` section text, verbatim.
    pub section: &'static str,
    /// The init file's path — what a later provisioner (the MCP wire)
    /// bakes into its own init as the literal predecessor, so both
    /// files load whichever one `BASH_ENV` names.
    pub init_path: PathBuf,
    dir: PathBuf,
}

impl Drop for Wiring {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

static WIRED: OnceLock<Option<Arc<Wiring>>> = OnceLock::new();

/// The process's wiring, built on first use and kept for the process's
/// life. `bash_env` is the pre-existing `BASH_ENV`, if any — the init
/// file sources it first and guarded, so a user's own `BASH_ENV` chain
/// survives under the generated one. `None` only when provisioning
/// failed (the answer is cached; a failed temp dir does not retry).
///
/// The production seam: the binary exec'd is `current_exe()`. Tests
/// drive [`provision_with_bin`] directly instead — the process-wide
/// cache makes [`wire`] unusable inside a shared test process.
pub fn wire(bash_env: Option<OsString>) -> Option<&'static Arc<Wiring>> {
    WIRED
        .get_or_init(|| {
            let bin = std::env::current_exe().ok()?;
            provision_with_bin(bash_env, &bin).map(Arc::new).ok()
        })
        .as_ref()
}

/// Provisions one process: the 0700 dir, the init file, the staging
/// directory, the env map. `bin` is the binary the function execs —
/// production passes `current_exe()`, tests pass whichever target
/// speaks [`MODE`].
///
/// # Errors
///
/// Filesystem failures: the dir cannot be created or the init file
/// cannot be written.
pub fn provision_with_bin(bash_env: Option<OsString>, bin: &Path) -> std::io::Result<Wiring> {
    let dir = std::env::temp_dir().join(format!(
        "rhema-vision-{}-{}",
        std::process::id(),
        // Nanos of the call: production provisions once per process,
        // but tests provision repeatedly in one process and each
        // Wiring owns its dir exclusively (Drop removes it).
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
    }
    let staging = dir.join("staging");
    std::fs::create_dir_all(&staging)?;

    let init_path = dir.join("init.bash");
    let mut text = String::from("# generated by rhema-agent-tools; do not edit\n");
    // The predecessor, first and guarded: `source ... || true` keeps a
    // user file's `set -e` failure from aborting the init, the same
    // reviewed shape the MCP init uses. The path is baked into the
    // file, never carried by an env var: the executor layers the map
    // onto every command, so a var reaches *every* generated init in a
    // chain and an outer one re-sources itself until bash dies by
    // signal — the shape any process with BASH_ENV already set
    // produced. A literal path sources exactly once. Provision paths
    // carry no user input; the one quote that could break single
    // quoting is escaped anyway.
    if let Some(inherited) = bash_env {
        let quoted = inherited.to_string_lossy().replace('\'', "'\\\''");
        text.push_str(&format!("source '{quoted}' || true\n"));
    }
    text.push_str(&format!(
        "{NAME}() {{ \"$RHEMA_VISION_BIN\" {MODE} \"$@\"; }}\n"
    ));
    std::fs::write(&init_path, text)?;

    let mut shell_env = BTreeMap::new();
    shell_env.insert(VAR_BASH_ENV.to_owned(), init_path.display().to_string());
    shell_env.insert(VAR_BIN.to_owned(), bin.display().to_string());
    shell_env.insert(VAR_STAGING.to_owned(), staging.display().to_string());

    Ok(Wiring {
        shell_env,
        section: PROMPT_SECTION,
        init_path,
        dir,
    })
}

/// The `__vision` mode's entry: stage every argument path, or stdin
/// when no path is given. The exit code is the process's: 0 staged, 1
/// empty stdin, 2 refused bytes (unrecognized, oversized, missing
/// path), 3 an I/O failure, 4 no staging directory in the environment
/// — a shell the wiring never reached.
pub fn cli_run(argv: &[OsString]) -> i32 {
    let Some(staging) = std::env::var_os(VAR_STAGING) else {
        eprintln!("{NAME}: not wired in this shell (no {VAR_STAGING})");
        return 4;
    };
    let staging = PathBuf::from(staging);
    let paths: Vec<OsString> = argv.iter().skip(1).cloned().collect();
    let mut failed = 0;
    if paths.is_empty() {
        let mut bytes = Vec::new();
        if let Err(err) = std::io::stdin()
            .take((MAX_IMAGE_BYTES + 1) as u64)
            .read_to_end(&mut bytes)
        {
            eprintln!("{NAME}: reading stdin failed: {err}");
            return 3;
        }
        return match stage_bytes(&staging, &bytes, "0") {
            Ok(_) => {
                println!("[image staged for viewing: {} bytes]", bytes.len());
                0
            }
            Err(err) => {
                eprintln!("{NAME}: {err}");
                exit_code(&err)
            }
        };
    }
    for (index, path) in paths.iter().enumerate() {
        let bytes = match std::fs::read(path) {
            Ok(bytes) => bytes,
            Err(err) => {
                eprintln!("{NAME}: {}: {err}", path.display());
                failed += 1;
                continue;
            }
        };
        match stage_bytes(&staging, &bytes, &index.to_string()) {
            Ok(_) => println!("[image staged for viewing: {}]", path.display()),
            Err(err) => {
                eprintln!("{NAME}: {}: {err}", path.display());
                failed += 1;
            }
        }
    }
    if failed > 0 { 2 } else { 0 }
}

fn exit_code(err: &StageError) -> i32 {
    match err {
        StageError::Empty => 1,
        StageError::NotAnImage | StageError::TooLarge(_) => 2,
        StageError::Io(_) => 3,
    }
}

/// Stages one image: sniff, size check, write under a name only this
/// process would reuse. The extension stays `.img` — the type lives in
/// the bytes, and [`attach`] sniffs again, so a mislabeled file cannot
/// mislead anyone.
///
/// # Errors
///
/// [`StageError`]: empty input, unrecognized format, over
/// [`MAX_IMAGE_BYTES`], or a filesystem failure.
pub fn stage_bytes(
    staging: &Path,
    bytes: &[u8],
    disambiguator: &str,
) -> Result<PathBuf, StageError> {
    if bytes.is_empty() {
        return Err(StageError::Empty);
    }
    if bytes.len() > MAX_IMAGE_BYTES {
        return Err(StageError::TooLarge(bytes.len()));
    }
    if ImageMediaType::sniff(bytes).is_none() {
        return Err(StageError::NotAnImage);
    }
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let path = staging.join(format!(
        "{}-{nanos}-{disambiguator}.img",
        std::process::id()
    ));
    std::fs::write(&path, bytes)?;
    Ok(path)
}

/// Drains the staging directory into `result`: every recognizable,
/// in-cap image joins [`ToolResult::images`], every staged file is
/// consumed, and what could not attach is said in the result's text.
/// Idempotent by construction — the directory empties — so a result
/// that runs twice attaches once.
pub(crate) fn attach(staging: &Path, result: &mut ToolResult) {
    let Ok(entries) = std::fs::read_dir(staging) else {
        return;
    };
    let mut paths: Vec<PathBuf> = entries.flatten().map(|entry| entry.path()).collect();
    paths.sort();
    let mut attached: Vec<Image> = Vec::new();
    let mut dropped: Vec<String> = Vec::new();
    for path in paths {
        // Consume unconditionally: a file we cannot read is not left to
        // fail the next result too.
        let bytes = std::fs::read(&path).ok();
        let _ = std::fs::remove_file(&path);
        let Some(bytes) = bytes else {
            continue;
        };
        if bytes.len() > MAX_IMAGE_BYTES {
            dropped.push(format!(
                "{} bytes over the {} byte limit",
                bytes.len(),
                MAX_IMAGE_BYTES
            ));
            continue;
        }
        let Some(media_type) = ImageMediaType::sniff(&bytes) else {
            dropped.push(format!("{} bytes, not a recognized image", bytes.len()));
            continue;
        };
        if attached.len() == MAX_IMAGES_PER_RESULT {
            dropped.push(format!(
                "1 more image ({}) dropped: at most {MAX_IMAGES_PER_RESULT} attach per result",
                media_type.as_str()
            ));
            continue;
        }
        attached.push(Image::new(
            media_type,
            base64::engine::general_purpose::STANDARD.encode(&bytes),
        ));
    }
    if attached.is_empty() && dropped.is_empty() {
        return;
    }
    let count = attached.len();
    result.images.extend(attached);
    result.content.push_str(&format!(
        "\n[{count} image(s) attached and visible to you{}]",
        if dropped.is_empty() {
            String::new()
        } else {
            format!("; dropped: {}", dropped.join("; "))
        }
    ));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal valid PNG: the 8-byte magic plus an IHDR chunk's
    /// length and type — enough for the sniffer, nothing more.
    fn png_bytes() -> Vec<u8> {
        let mut bytes = b"\x89PNG\r\n\x1a\n".to_vec();
        bytes.extend_from_slice(&[0, 0, 0, 13]);
        bytes.extend_from_slice(b"IHDR");
        bytes
    }

    #[test]
    fn staging_sniffs_and_names_uniquely() {
        let dir = tempfile::tempdir().unwrap();
        let first = stage_bytes(dir.path(), &png_bytes(), "0").unwrap();
        let second = stage_bytes(dir.path(), &png_bytes(), "1").unwrap();
        assert_ne!(first, second);
        assert_eq!(std::fs::read(&first).unwrap(), png_bytes());

        let err = stage_bytes(dir.path(), b"definitely not an image", "2").unwrap_err();
        assert!(err.to_string().contains("not a recognized image"), "{err}");
        let err = stage_bytes(dir.path(), &[], "3").unwrap_err();
        assert!(err.to_string().contains("no bytes"), "{err}");
        let err = stage_bytes(dir.path(), &vec![0u8; MAX_IMAGE_BYTES + 1], "4").unwrap_err();
        assert!(err.to_string().contains("too large"), "{err}");
    }

    #[test]
    fn attach_moves_staged_images_into_the_result_and_empties_the_dir() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.img"), png_bytes()).unwrap();
        std::fs::write(dir.path().join("b.img"), b"not an image").unwrap();

        let mut result = ToolResult::success("c1", "out");
        attach(dir.path(), &mut result);
        assert_eq!(result.images.len(), 1);
        assert_eq!(result.images[0].media_type, ImageMediaType::Png);
        assert!(
            result.content.contains("1 image(s) attached"),
            "{}",
            result.content
        );
        assert!(
            result.content.contains("not a recognized image"),
            "{}",
            result.content
        );
        assert!(
            std::fs::read_dir(dir.path()).unwrap().next().is_none(),
            "the staging dir empties"
        );

        // A second attach is a no-op: the note does not repeat.
        attach(dir.path(), &mut result);
        assert_eq!(result.images.len(), 1);
        assert_eq!(result.content.matches("attached").count(), 1);
    }

    #[test]
    fn attach_caps_the_images_per_result() {
        let dir = tempfile::tempdir().unwrap();
        for index in 0..=MAX_IMAGES_PER_RESULT {
            std::fs::write(dir.path().join(format!("{index}.img")), png_bytes()).unwrap();
        }
        let mut result = ToolResult::success("c1", "out");
        attach(dir.path(), &mut result);
        assert_eq!(result.images.len(), MAX_IMAGES_PER_RESULT);
        assert!(
            result
                .content
                .contains(&format!("at most {MAX_IMAGES_PER_RESULT} attach")),
            "{}",
            result.content
        );
    }

    #[test]
    fn an_unreadable_staged_file_drops_without_poisoning_the_result() {
        let dir = tempfile::tempdir().unwrap();
        // A directory masquerading as a staged file: read fails, remove
        // fails, and the attach must neither panic nor note it.
        std::fs::create_dir(dir.path().join("dir.img")).unwrap();
        let mut result = ToolResult::success("c1", "out");
        attach(dir.path(), &mut result);
        assert_eq!(result.images, Vec::new());
        assert_eq!(result.content, "out");
    }

    #[test]
    fn the_init_file_bakes_the_predecessor_in_and_defines_the_function() {
        let wiring = provision_with_bin(
            Some(OsString::from("/user/init.bash")),
            Path::new("/usr/local/bin/rhema"),
        )
        .unwrap();
        let text = std::fs::read_to_string(&wiring.init_path).unwrap();
        assert!(
            text.contains("source '/user/init.bash' || true\n"),
            "{text}"
        );
        assert!(
            text.contains(&format!(
                "{NAME}() {{ \"$RHEMA_VISION_BIN\" {MODE} \"$@\"; }}"
            )),
            "{text}"
        );
        // The chain rides the file text, never the env map: an env var
        // reaches every generated init in a chain and an outer one
        // re-sources itself until the shell dies.
        assert!(
            !wiring.shell_env.contains_key("RHEMA_INHERITED_BASH_ENV"),
            "{:?}",
            wiring.shell_env
        );
        let staging = PathBuf::from(wiring.shell_env.get(VAR_STAGING).unwrap());
        assert!(staging.is_dir(), "{staging:?} exists");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let dir = staging.parent().unwrap();
            let mode = std::fs::metadata(dir).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o700, "the dir is 0700, got {mode:o}");
        }

        // No predecessor, no guard line at all.
        let bare = provision_with_bin(None, Path::new("/usr/local/bin/rhema")).unwrap();
        let text = std::fs::read_to_string(&bare.init_path).unwrap();
        assert_eq!(
            text,
            "# generated by rhema-agent-tools; do not edit\nsee() { \"$RHEMA_VISION_BIN\" __vision \"$@\"; }\n"
        );
    }

    #[test]
    fn the_sourced_chain_terminates_where_an_env_var_chain_would_recurse() {
        // The nested-process shape: the environment already carries a
        // BASH_ENV naming a generated init (an outer session's) that
        // guards on the same var the executor used to layer. The
        // executor must set no such var, so the outer file's guard
        // reads empty and the chain stops after one hop.
        let outer = tempfile::tempdir().unwrap();
        let outer_init = outer.path().join("outer-init.bash");
        std::fs::write(
            &outer_init,
            "[ -n \"${RHEMA_INHERITED_BASH_ENV:-}\" ] && \\\n  source \"$RHEMA_INHERITED_BASH_ENV\" || true\nouter() { :; }\n",
        )
        .unwrap();

        let wiring = provision_with_bin(
            Some(OsString::from(outer_init.display().to_string())),
            Path::new("/usr/local/bin/rhema"),
        )
        .unwrap();
        let inner_init = wiring.init_path.display().to_string();
        let staging = wiring.shell_env.get(VAR_STAGING).unwrap().clone();
        // The command env, as the executor layers it: our map only.
        let script = format!(
            "env BASH_ENV={inner_init:?} RHEMA_VISION_BIN=/usr/bin/true RHEMA_VISION_STAGING={staging:?} bash -c 'echo alive'"
        );
        let output = std::process::Command::new("bash")
            .arg("-c")
            .arg(&script)
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "alive");
    }
}
