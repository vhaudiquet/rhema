//! The `decide` macro: how the model asks a decision model from the
//! shell.
//!
//! A decision model (TypeSafe's Jev, Cloudflare's Clef, or one
//! OpenRouter routes to) answers typed questions with probabilities
//! and generates no text, so the answers are shell data like any
//! other command's output, and the surface is a bash function the
//! model calls directly:
//!
//! ```sh
//! decide noul "the order never arrived" "Does the customer want a person?"
//! decide choice "the order never arrived" "Which team" billing=Payments shipping=Delivery
//! decide <<'JSON'
//! { "state": "…", "questions": { "urgent": { "type": "noul", "instructions": "…" } } }
//! JSON
//! ```
//!
//! One half, the same provision the other macros ride:
//!
//! - [`provision_with_bin`] writes the init file one non-interactive
//!   bash sources from `BASH_ENV` (the same fact the jobs, vision,
//!   and search provisions rest on, `specs/rhema-mcp.md` §2):
//!   `decide()` execs the rhema binary in its `__decision` mode, whose
//!   handler is the frontend's, not this crate's. The wiring's env map
//!   carries the init file's path, the binary's, and the session key's;
//!   the frontend builds it once per session and layers the map over
//!   `HostAgentConfig::shell_env`.
//!
//! Two properties make this half empty, the search macro's:
//!
//! - No staging directory and no attach pass: answers ride stdout
//!   like every other command's output, under the shell tool's caps.
//! - No keys and no settings here: the `__decision` mode resolves
//!   them in-process from the profile [`VAR_PROFILE`] names. The
//!   agent's shell sees the profile's name, never a key.
//!
//! A remote sandbox with the installed rhema helper wires the same
//! function: the init file execs the helper, and the host injects the
//! profile's resolved instance. The sandbox never sees the keychain or
//! the config file, only the instance the host built. A sandbox
//! without the helper wires nothing.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

/// The bash function's name, and the model's whole interface. A rename
/// is this constant plus the prompt section and the spec text.
pub const NAME: &str = "decide";

/// The hidden binary mode the init file's function execs: `rhema
/// __decision …`. The public interface is the function; the mode is a
/// transport detail, like the search mode's `__search`.
pub const MODE: &str = "__decision";

/// The env var non-interactive bash reads its init file from, the
/// same var the jobs, vision, search, and MCP provisions use, so
/// whichever init file the wiring names last, its baked predecessor
/// line reaches the others.
pub use crate::init::VAR_BASH_ENV;
/// The env var holding the binary the function execs.
pub const VAR_BIN: &str = "RHEMA_DECISION_BIN";
/// The env var holding the profile whose decision provider the
/// `__decision` mode resolves. Keys never ride the shell: this is the
/// only setting the function's side of the wire carries.
pub const VAR_PROFILE: &str = "RHEMA_DECISION_PROFILE";

/// The exit code for a request the mode cannot serve as typed: no
/// verb, an unknown flag, a shorthand missing its words, JSON that is
/// not a request. The `__decision` handler returns it; the model reads
/// the usage text on stderr.
pub const EXIT_USAGE: i32 = 2;
/// The exit code for a request refused as invalid: the seam's own
/// validation or a vendor's 400. Distinct from a provider
/// failure, because the fix is in the request, not in a retry.
pub const EXIT_INVALID: i32 = 3;
/// The exit code for a shell the wiring never reached, no
/// `RHEMA_DECISION_PROFILE` in the environment, the other modes'
/// convention.
pub const EXIT_NOT_WIRED: i32 = 4;

/// The system-prompt section a wired frontend renders: the macro's
/// whole documentation, as [`include_str!`] of the file beside this
/// module, the same crate-root data file the system template ships
/// as. No trailing newline: the template interpolates the bytes
/// verbatim.
pub const PROMPT_SECTION: &str = include_str!("../SYSTEM_DECISION.md");

/// One session's wiring: the env map a frontend layers over
/// `HostAgentConfig::shell_env`, the prompt section to render, and
/// the provisioned init file held for the process's life.
pub struct Wiring {
    /// The env map: `BASH_ENV`, `RHEMA_DECISION_BIN`, and
    /// `RHEMA_DECISION_PROFILE`. Deliberately no inherited-`BASH_ENV`
    /// entry: the lineage is baked into the init file's text (each
    /// file sources its predecessor by literal path), and an env var
    /// here would make an outer generated init re-source itself
    /// until the shell died.
    pub shell_env: BTreeMap<String, String>,
    /// The prompt's `decision` section text, verbatim.
    pub section: &'static str,
    /// The init file's path: what the next provisioner (the MCP wire)
    /// bakes into its own init as the literal predecessor, so every
    /// generated file loads in one lineage.
    pub init_path: PathBuf,
    dir: PathBuf,
}

impl Drop for Wiring {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

static WIRED: OnceLock<Option<Arc<Wiring>>> = OnceLock::new();

/// The process's wiring, built on first use and kept for the
/// process's life. The answer is cached: a frontend that provisions
/// per session calls [`provision_with_bin`] instead, the search
/// wire's shape, since the profile name is per session.
///
/// The production seam: the binary exec'd is `current_exe()`. Tests
/// drive [`provision_with_bin`] directly instead, because the
/// process-wide cache makes [`wire`] unusable inside a shared test
/// process.
pub fn wire(bash_env: Option<OsString>, profile: &str) -> Option<&'static Arc<Wiring>> {
    WIRED
        .get_or_init(|| {
            let bin = std::env::current_exe().ok()?;
            provision_inner(bash_env, &bin, profile, false)
                .map(Arc::new)
                .ok()
        })
        .as_ref()
}

/// Provisions one session: the 0700 dir, the init file, the env map.
/// `bin` is the binary the function execs; production passes
/// `current_exe()`, tests pass whichever target speaks [`MODE`].
///
/// # Errors
///
/// Filesystem failures: the dir cannot be created or the init file
/// cannot be written.
pub fn provision_with_bin(
    bash_env: Option<OsString>,
    bin: &Path,
    profile: &str,
) -> std::io::Result<Wiring> {
    provision_inner(bash_env, bin, profile, true)
}

/// The state provision; `write_init` is this module's test seam (see
/// [`crate::jobs`]).
fn provision_inner(
    bash_env: Option<OsString>,
    bin: &Path,
    profile: &str,
    write_init: bool,
) -> std::io::Result<Wiring> {
    let dir = crate::init::temp_dir("rhema-decision")?;
    let (init_path, mut shell_env) = if write_init {
        let body = crate::init::function(NAME, VAR_BIN, MODE);
        let path = crate::init::write(&dir, "rhema-bash", bash_env, &body)?;
        let env = crate::init::env_with(&path);
        (path, env)
    } else {
        (dir.join("init.bash"), std::collections::BTreeMap::new())
    };
    shell_env.insert(VAR_BIN.to_owned(), bin.display().to_string());
    shell_env.insert(VAR_PROFILE.to_owned(), profile.to_owned());

    Ok(Wiring {
        shell_env,
        section: PROMPT_SECTION,
        init_path,
        dir,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A target that speaks `MODE` in place of the real binary: the
    /// tests' exec stand-in, the other macros' convention.
    fn fake_bin(dir: &Path) -> PathBuf {
        let bin = dir.join("rhema-fake");
        std::fs::write(&bin, "#!/bin/sh\nexit 0\n").unwrap();
        bin
    }

    #[test]
    fn the_init_file_sources_the_predecessor_then_defines_the_function() {
        let dir = tempfile::tempdir().unwrap();
        let wiring = provision_with_bin(
            Some(OsString::from("/tmp/rhema-search-1/init.bash")),
            &fake_bin(dir.path()),
            "personal",
        )
        .unwrap();
        let text = std::fs::read_to_string(&wiring.init_path).unwrap();
        // The predecessor line, literal and guarded, first.
        assert!(
            text.starts_with("# generated by rhema-bash; do not edit\nsource '/tmp/rhema-search-1/init.bash' || true\n"),
            "{text}"
        );
        // The function line, execing the binary in the hidden mode.
        assert!(
            text.contains("decide() { \"$RHEMA_DECISION_BIN\" __decision \"$@\"; }\n"),
            "{text}"
        );
    }

    #[test]
    fn a_bare_provision_has_no_predecessor_line() {
        let dir = tempfile::tempdir().unwrap();
        let wiring = provision_with_bin(None, &fake_bin(dir.path()), "personal").unwrap();
        let text = std::fs::read_to_string(&wiring.init_path).unwrap();
        assert_eq!(
            text,
            "# generated by rhema-bash; do not edit\ndecide() { \"$RHEMA_DECISION_BIN\" __decision \"$@\"; }\n"
        );
    }

    #[test]
    fn the_env_map_carries_the_lineage_and_never_a_key() {
        let dir = tempfile::tempdir().unwrap();
        let bin = fake_bin(dir.path());
        let wiring = provision_with_bin(None, &bin, "work").unwrap();
        assert_eq!(
            wiring.shell_env.get(VAR_BASH_ENV).map(String::as_str),
            Some(wiring.init_path.display().to_string().as_str())
        );
        assert_eq!(
            wiring.shell_env.get(VAR_BIN).map(String::as_str),
            Some(bin.display().to_string().as_str())
        );
        assert_eq!(
            wiring.shell_env.get(VAR_PROFILE).map(String::as_str),
            Some("work")
        );
        // Three entries, no staging, no key carrier.
        assert_eq!(wiring.shell_env.len(), 3);
        assert!(wiring.shell_env.keys().all(|k| !k.contains("KEY")));
    }

    #[test]
    fn the_section_ships_verbatim_from_the_data_file() {
        let dir = tempfile::tempdir().unwrap();
        let wiring = provision_with_bin(None, &fake_bin(dir.path()), "personal").unwrap();
        assert_eq!(wiring.section, PROMPT_SECTION);
        assert_eq!(wiring.section, include_str!("../SYSTEM_DECISION.md"));
        assert!(wiring.section.contains("decide noul"), "{}", wiring.section);
        assert!(!wiring.section.ends_with('\n'), "no trailing newline");
    }

    #[test]
    fn two_provisions_own_two_dirs_and_drop_removes_theirs() {
        let dir = tempfile::tempdir().unwrap();
        let first = provision_with_bin(None, &fake_bin(dir.path()), "a").unwrap();
        let second = provision_with_bin(None, &fake_bin(dir.path()), "b").unwrap();
        assert_ne!(first.init_path, second.init_path);
        let first_dir = first.dir.clone();
        drop(first);
        assert!(!first_dir.exists(), "drop removes the provisioned dir");
        drop(second);
    }
}
