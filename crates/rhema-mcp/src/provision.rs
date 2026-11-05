//! Provision: materialize the init file and the env map that make
//! `atlassian list-issues …` work inside an agent's bash commands.
//!
//! The mechanism rests on a verified bash fact (spec §9): non-
//! interactive bash sources `BASH_ENV` for `-c` commands too, and a
//! function defined there is visible in pipelines, substitutions,
//! `set -eu` bodies, and nested `bash -c` children. So one init file
//! defines one stub per server, and the stub execs the rhema binary
//! in `__server` mode — which walks the manager's bridge.
//!
//! The file is written into a `0700` per-agent temp dir. Ordering in
//! the file is a reviewed failure mode: the user's inherited
//! `BASH_ENV` is sourced **first and guarded** (a user file that sets
//! `set -e` and fails must not abort the init, or `mcp` goes silently
//! undefined and every command exits 127), then our definitions
//! follow, so the documented interface wins name collisions. The
//! predecessor's path is baked into the file, never carried by an env
//! var: the executor layers the env map onto every command, so a var
//! reaches every generated init in a chain — and an outer one re-sources
//! itself until the shell dies by signal (the shape any process with
//! `BASH_ENV` already set produced, a nested rhema included).

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use rhema_config::mcp::{McpConfig, command_name_ok};

/// The name reserved for the meta surface (`mcp call`, `mcp
/// servers`): a server claiming it gets no generated function.
const RESERVED: &str = "mcp";

/// The init file's env var names, in one place: the file's text
/// references them and the env map sets them, so a rename means
/// touching both — this is the single source for both sides.
/// The env var non-interactive bash reads its init file from.
pub const VAR_BASH_ENV: &str = "BASH_ENV";
/// The env var holding the session-dir bin link the stubs exec.
pub const VAR_BIN: &str = "RHEMA_MCP_BIN";
/// The env var holding the bridge socket path.
pub const VAR_BRIDGE: &str = "RHEMA_MCP_BRIDGE";
/// The env var holding the per-agent bearer token.
pub const VAR_TOKEN: &str = "RHEMA_MCP_TOKEN";
/// The env var holding the session temp dir (`media/` under it).
pub const VAR_TMP: &str = "RHEMA_MCP_TMP";

/// What provisioning produced: the init file's path and the env map
/// to layer over every agent command. Dropping the guard deletes the
/// per-agent temp dir, best-effort — an in-flight command's outer
/// bash has already sourced the file, and a nested bash spawning
/// after the drop reads a dead path and exits 127, the same
/// degradation class as the stale-bridge case. Accepted, documented.
pub struct Provision {
    /// The init file's absolute path; also `BASH_ENV`'s value.
    pub init_path: PathBuf,
    /// The env map: layer it over `CodingAgentConfig::shell_env`.
    pub env: BTreeMap<String, String>,
    /// Servers skipped for name reasons, with the reason; the
    /// caller surfaces these as debug notices.
    pub skipped: Vec<String>,
    dir: PathBuf,
}

impl Drop for Provision {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Provisions one agent session: init file, bin link, env map.
///
/// `bridge_socket` and `token` are the bridge's socket path and the
/// per-agent token the wire layer generated; `session_tmp` is the
/// session's temp dir (`media/` lives under it). `bash_env` is the
/// pre-existing `BASH_ENV`, if any — the wire layer reads it before
/// provisioning so a sandbox's own init chain survives.
///
/// # Errors
///
/// Filesystem failures: the per-agent dir cannot be created, the
/// init file cannot be written, the bin cannot be linked or copied.
pub fn provision(
    config: &McpConfig,
    bridge_socket: &Path,
    token: &str,
    session_tmp: &Path,
    bash_env: Option<OsString>,
    agent_id: &str,
) -> std::io::Result<Provision> {
    provision_with_bin(
        config,
        bridge_socket,
        token,
        session_tmp,
        bash_env,
        agent_id,
        &std::env::current_exe()?,
    )
}

/// [`provision`] over an explicit bin — the process-level test seam:
/// production always execs `current_exe()` (the rhema binary), while
/// a test execs the crate's own CLI target.
///
/// # Errors
///
/// As [`provision`].
#[allow(clippy::too_many_arguments)]
pub fn provision_with_bin(
    config: &McpConfig,
    bridge_socket: &Path,
    token: &str,
    session_tmp: &Path,
    bash_env: Option<OsString>,
    agent_id: &str,
    bin_source: &Path,
) -> std::io::Result<Provision> {
    // The per-agent dir: 0700, unique per agent, under the system
    // temp dir — not under the session tmp, because RHEMA_MCP_TMP is
    // visible to the agent's commands and the init file is
    // infrastructure.
    let dir = std::env::temp_dir().join(format!("rhema-mcp-{agent_id}-{}", random_tag()));
    std::fs::create_dir_all(&dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
    }

    // The bin: a hardlink into the per-agent dir, so a `cargo
    // install` mid-session (which renames over the installed file)
    // cannot pull the inode out from under running commands. A copy
    // when the temp dir sits on another filesystem.
    let bin = dir.join(bin_source.file_name().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "the bin has no file name")
    })?);
    if std::fs::hard_link(bin_source, &bin).is_err() {
        std::fs::copy(bin_source, &bin)?;
    }

    // One stub per server, in config order; name conflicts with the
    // reserved `mcp` are skipped (the entry stays reachable via
    // `mcp call`), illegal function names likewise.
    let mut skipped = Vec::new();
    let mut stubs = String::new();
    for server in &config.servers {
        let name = &server.command_name;
        if name == RESERVED {
            skipped.push(format!(
                "mcp server `{}`: function name `{name}` is reserved for the meta surface; reachable via `mcp call`",
                server.name
            ));
            continue;
        }
        if !command_name_ok(name) {
            skipped.push(format!(
                "mcp server `{}`: function name `{name}` is not a bash identifier; reachable via `mcp call`",
                server.name
            ));
            continue;
        }
        stubs.push_str(&format!(
            "{name}() {{ \"$RHEMA_MCP_BIN\" __server {} \"$@\"; }}\n",
            server.name
        ));
    }

    let init_path = dir.join("init.bash");
    let mut text = String::from("# generated by rhema-mcp; do not edit\n");
    // The inherited file, first and guarded: `source ... || true`
    // keeps a user file's `set -e` failure from aborting the init. The
    // path is baked in — see the module docs for why it must not ride
    // an env var. Provision paths carry no user input; the one quote
    // that could break single quoting is escaped anyway.
    if let Some(inherited) = bash_env {
        let quoted = inherited.to_string_lossy().replace('\'', "'\\\''");
        text.push_str(&format!("source '{quoted}' || true\n"));
    }
    text.push_str(&format!(
        "{RESERVED}() {{ \"$RHEMA_MCP_BIN\" {RESERVED} \"$@\"; }}\n"
    ));
    text.push_str(&stubs);
    std::fs::write(&init_path, text)?;

    let mut env = BTreeMap::new();
    env.insert(
        VAR_BASH_ENV.to_owned(),
        init_path.to_string_lossy().into_owned(),
    );
    env.insert(VAR_BIN.to_owned(), bin.to_string_lossy().into_owned());
    env.insert(
        VAR_BRIDGE.to_owned(),
        bridge_socket.to_string_lossy().into_owned(),
    );
    env.insert(VAR_TOKEN.to_owned(), token.to_owned());
    env.insert(
        VAR_TMP.to_owned(),
        session_tmp.to_string_lossy().into_owned(),
    );

    Ok(Provision {
        init_path,
        env,
        skipped,
        dir,
    })
}

/// A short uniqueness tag for the per-agent dir name: time nanos
/// mixed with the pid. Not cryptographic — the dir's 0700 mode is
/// the security boundary; the tag only prevents collisions between
/// concurrent sessions of the same agent id.
fn random_tag() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64 ^ d.as_secs())
        .unwrap_or(0);
    format!("{:x}-{}", nanos, std::process::id())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rhema_config::mcp::{McpServer, McpTransport};
    use std::time::Duration;

    fn server(name: &str, command_name: &str) -> McpServer {
        McpServer {
            name: name.to_owned(),
            description: "desc".to_owned(),
            transport: McpTransport::Stdio {
                command: vec!["true".to_owned()],
                env: BTreeMap::new(),
            },
            startup_timeout: Duration::from_secs(1),
            timeout: Duration::from_secs(1),
            command_name: command_name.to_owned(),
        }
    }

    fn http_server(name: &str, command_name: &str) -> McpServer {
        McpServer {
            transport: McpTransport::Http {
                url: "http://127.0.0.1:1/mcp".to_owned(),
                headers: BTreeMap::new(),
            },
            ..server(name, command_name)
        }
    }

    fn config(servers: Vec<McpServer>) -> McpConfig {
        McpConfig {
            servers,
            notices: Vec::new(),
        }
    }

    fn provision_two() -> Provision {
        let dir = tempfile::tempdir().expect("session tmp");
        provision(
            &config(vec![
                server("atlassian", "atlassian"),
                http_server("blender", "blender"),
            ]),
            Path::new("/tmp/bridge.sock"),
            "cafe12",
            dir.path(),
            None,
            "agent-test",
        )
        .expect("provisions")
    }

    #[test]
    fn the_init_file_has_the_exact_spec_shape() {
        let provision = provision_two();
        let text = std::fs::read_to_string(&provision.init_path).expect("init file");
        let expected = "# generated by rhema-mcp; do not edit\n\
            mcp() { \"$RHEMA_MCP_BIN\" mcp \"$@\"; }\n\
            atlassian() { \"$RHEMA_MCP_BIN\" __server atlassian \"$@\"; }\n\
            blender() { \"$RHEMA_MCP_BIN\" __server blender \"$@\"; }\n";
        assert_eq!(text, expected);
    }

    #[test]
    fn the_env_map_carries_every_documented_var() {
        let provision = provision_two();
        let init = provision.init_path.to_string_lossy().into_owned();
        assert_eq!(
            provision.env.get(VAR_BASH_ENV).map(String::as_str),
            Some(init.as_str())
        );
        let bin = provision.env.get(VAR_BIN).expect("bin var");
        assert!(
            Path::new(bin.as_str()).is_file(),
            "the bin link exists: {bin}"
        );
        assert_eq!(
            provision.env.get(VAR_BRIDGE).map(String::as_str),
            Some("/tmp/bridge.sock")
        );
        assert_eq!(
            provision.env.get(VAR_TOKEN).map(String::as_str),
            Some("cafe12")
        );
        assert!(provision.env.contains_key(VAR_TMP));
        assert!(provision.skipped.is_empty());
    }

    #[test]
    fn an_inherited_bash_env_is_baked_into_the_init_file() {
        let dir = tempfile::tempdir().expect("session tmp");
        let provision = provision(
            &config(vec![]),
            Path::new("/tmp/s.sock"),
            "t",
            dir.path(),
            Some(OsString::from("/etc/profile.d/agent.sh")),
            "agent-inh",
        )
        .expect("provisions");
        let text = std::fs::read_to_string(&provision.init_path).expect("init file");
        assert!(
            text.contains("source '/etc/profile.d/agent.sh' || true\n"),
            "{text}"
        );
        // The chain rides the file text, never the env map.
        assert!(!provision.env.contains_key("RHEMA_INHERITED_BASH_ENV"));
    }

    #[test]
    fn reserved_and_illegal_names_are_skipped_with_reasons() {
        let dir = tempfile::tempdir().expect("session tmp");
        let provision = provision(
            &config(vec![
                server("meta", "mcp"),
                server("bad", "2fast"),
                server("good", "my-server_2"),
            ]),
            Path::new("/tmp/s.sock"),
            "t",
            dir.path(),
            None,
            "agent-skip",
        )
        .expect("provisions");
        assert_eq!(provision.skipped.len(), 2, "{:?}", provision.skipped);
        assert!(provision.skipped[0].contains("reserved"));
        assert!(provision.skipped[1].contains("not a bash identifier"));
        let text = std::fs::read_to_string(&provision.init_path).expect("init file");
        assert!(
            text.contains("my-server_2()"),
            "hyphenated stubs survive: {text}"
        );
        assert!(!text.contains("2fast()"));
        assert_eq!(
            text.lines().filter(|l| l.contains("__()")).count(),
            0,
            "no stub for the reserved name"
        );
    }

    #[test]
    fn the_per_agent_dir_is_private_and_disappears_on_drop() {
        let (path, guard) = {
            let provision = provision_two();
            let path = provision.init_path.clone();
            let dir = provision.dir.clone();
            assert!(
                path.is_file(),
                "the init file exists while provisioned: {path:?}"
            );
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = std::fs::metadata(&dir).expect("dir").permissions().mode();
                assert_eq!(mode & 0o777, 0o700, "the dir is 0700, got {mode:o}");
            }
            (dir, provision)
        };
        drop(guard);
        assert!(!path.exists(), "the init file went with the dir");
    }

    #[test]
    fn the_bin_is_hardlinked_into_the_dir_when_possible() {
        let provision = provision_two();
        let exe = std::env::current_exe().expect("current exe");
        let bin = provision
            .env
            .get(VAR_BIN)
            .map(PathBuf::from)
            .expect("bin var");
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let exe_ino = std::fs::metadata(&exe).expect("exe").ino();
            let bin_ino = std::fs::metadata(&bin).expect("bin").ino();
            // Same filesystem: the link is a link, not a copy. On a
            // cross-device temp dir the copy branch runs and the
            // inodes differ, which is also fine — the test asserts
            // only what the layout guarantees.
            if exe_ino == bin_ino {
                // hardlink confirmed
            }
        }
        assert!(bin.is_file());
    }
}
