//! The SSH backend: commands on a remote box, no daemon on the far
//! end, and no `ssh` binary assumed on this one.
//!
//! [`SshSandbox`] is the fs-disjoint family's daemonless member (spec
//! §4's SSH row, first variant): zero setup beyond an ssh login. The
//! client is built in ([`russh`]), so a host that cannot run `ssh`
//! still reaches a remote box; authentication hunts the same places
//! the binary would (an explicit key, the agent, the config's
//! `IdentityFile`s, the default key files) and the destination
//! resolves against the user's own `~/.ssh/config`. Exec is
//! `timeout --kill-after=5 <secs> bash -c <command>` over a session
//! channel, the same prefix, so the same budget knob and the same
//! timeout reading (124 at the budget; an external SIGKILL reads as
//! the out-of-memory error), with the streams pumped live through
//! the shared `run_child` wiring's semantics. Files cross as `cat`
//! and `tee`; seeding is a remote `git clone` or a tarball streamed
//! raw to a remote `tar -xzf -`. Every fs operation crosses the
//! boundary, which is what makes the family different, there is no
//! local root to resolve against, so the model-influenced paths are
//! guarded textually by `check_remote_path` (`exec_util`).
//!
//! The host-key check is `accept-new` (first contact learns,
//! a changed key refuses), the daemon-friendly reading of OpenSSH's
//! default; the session module's docs carry the detail.
#![forbid(unsafe_code)]

#[cfg(feature = "test-util")]
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use async_trait::async_trait;
use base64::Engine;

use rhema_sandbox::exec_util::{EXEC_GRACE, check_remote_path, shquote};
use rhema_sandbox::{RunOutcome, RunRequest, Sandbox, SandboxError, ToolProgress, WorkspaceSeed};

use session::RemoteCall;

mod session;

/// The seed budget, the shim's, the same one the fs-sharing backends
/// give their seed commands. A clone or unpack past it is a timeout:
/// the connection drops, the remote command may finish.
const SEED_BUDGET: Duration = Duration::from_secs(300);

/// The budget of one fs call (`cat` / `tee` over a channel): the exec
/// routes carry their request's budget, the fs routes have none to
/// carry, and a black-holed transport would hang `read_file` or
/// `write_file` forever without a fixed one.
const FS_BUDGET: Duration = Duration::from_secs(60);

/// The SSH sandbox a session's commands run in: one remote login, and
/// nothing installed on the far end, that is the mode's reason to
/// exist.
///
/// `host` is an ssh destination; any alias or `user@host` the local
/// config resolves works, with an optional `:port` suffix. The remote
/// workspace root, where seeds land, defaults to `/root`; name
/// another directory with [`SshSandbox::with_workspace`]. A git clone
/// seeds into that directory, so as with a fresh cloud workspace it
/// must be empty or absent when the seed lands.
///
/// The credential rides [`SshSandbox::with_private_key`] as the key
/// text itself (OpenSSH or PEM format, no passphrase), not a path:
/// a daemon that holds its credentials in a config or a store has no
/// file to name. Without one, the hunt tries the agent and the
/// default key files, the way the `ssh` binary would, minus every
/// prompt (a daemon cannot answer one).
///
/// Cancelling mid-run drops the connection; the remote command may
/// finish on the far end, ssh carries no cancel, and this type does
/// not pretend otherwise.
#[derive(Debug, Clone)]
pub struct SshSandbox {
    /// The ssh destination, resolved against the local config on
    /// every call.
    host: String,
    /// The remote directory standing in as the workspace root.
    workspace: String,
    /// The key text, when the caller carries the credential itself
    /// instead of pointing at a file.
    private_key: Option<String>,
    /// The known-hosts override, the tests' seam (feature
    /// `test-util`): the real path is the user's own, or the
    /// config's `UserKnownHostsFile`.
    #[cfg(feature = "test-util")]
    known_hosts: Option<PathBuf>,
    /// The strictness override, the tests' seam (feature
    /// `test-util`): the real value is the config's
    /// `StrictHostKeyChecking`, else `accept-new`.
    #[cfg(feature = "test-util")]
    strict: Option<bool>,
    /// The config-file override, the tests' seam (feature
    /// `test-util`): the real resolution reads the user's own
    /// `~/.ssh/config`.
    #[cfg(feature = "test-util")]
    ssh_config: Option<PathBuf>,
}

impl SshSandbox {
    /// A sandbox on `host`, seeded into the remote `/root`.
    pub fn new(host: impl Into<String>) -> Self {
        Self {
            host: host.into(),
            workspace: "/root".to_owned(),
            private_key: None,
            #[cfg(feature = "test-util")]
            known_hosts: None,
            #[cfg(feature = "test-util")]
            strict: None,
            #[cfg(feature = "test-util")]
            ssh_config: None,
        }
    }

    /// Names the remote directory the workspace root is, where the
    /// seeds land, instead of the `/root` default.
    pub fn with_workspace(mut self, workspace: impl Into<String>) -> Self {
        self.workspace = workspace.into();
        self
    }

    /// Carries the private key itself: the OpenSSH or PEM text, not
    /// a path, so a caller whose credentials live in a config or a
    /// store has no file to stage. A blank value keeps the key
    /// unset, the same reading a config field's blank line gets.
    /// Encrypted keys are not this setter's cargo: a daemon cannot
    /// prompt for the passphrase, so hand it the decrypted text or
    /// let the agent hold the key instead.
    pub fn with_private_key(mut self, key: impl Into<String>) -> Self {
        let key = key.into();
        let trimmed = key.trim();
        if trimmed.is_empty() {
            return self;
        }
        self.private_key = Some(trimmed.to_owned());
        self
    }

    /// Points the host-key check at one `known_hosts` file instead
    /// of the user's own. The tests' seam (feature `test-util`): a
    /// test must not learn host keys into the runner's real
    /// `~/.ssh/known_hosts`.
    #[cfg(feature = "test-util")]
    pub fn with_known_hosts_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.known_hosts = Some(path.into());
        self
    }

    /// Forces the host-key strictness instead of reading the
    /// config's `StrictHostKeyChecking`. The tests' seam (feature
    /// `test-util`).
    #[cfg(feature = "test-util")]
    pub fn with_strict_host_keys(mut self, strict: bool) -> Self {
        self.strict = Some(strict);
        self
    }

    /// Points the destination resolution at one config file instead
    /// of the user's own `~/.ssh/config`. The tests' seam (feature
    /// `test-util`): a test must not read, or depend on, a real
    /// user's ssh config.
    #[cfg(feature = "test-util")]
    pub fn with_ssh_config_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.ssh_config = Some(path.into());
        self
    }

    /// Feeds one remote call `bytes` on its stdin (the end of input
    /// sent after) under `budget`, answering its exit code and its
    /// stderr text; the bytes cross as bytes, no line splitting,
    /// which is where the write route's binary-safety rides. A far
    /// end that will not speak is an unavailable sandbox; a far end
    /// that stops answering inside the budget is a timeout.
    async fn feed(
        &self,
        budget: Duration,
        command: &str,
        bytes: Vec<u8>,
    ) -> Result<(Option<i32>, String), SandboxError> {
        self.budgeted(budget, async {
            self.call(command).await?.feed(bytes).await
        })
        .await
    }

    /// One call under a wall-clock budget: past it the future
    /// drops, and with it the connection, a black-holed transport
    /// reads as a timeout, not a hang.
    async fn budgeted<T>(
        &self,
        budget: Duration,
        call: impl std::future::Future<Output = Result<T, SandboxError>>,
    ) -> Result<T, SandboxError> {
        match tokio::time::timeout(budget, call).await {
            Ok(answer) => answer,
            Err(_) => Err(SandboxError::Timeout(budget.as_secs())),
        }
    }

    /// Opens one remote call carrying `command`, with the sandbox's
    /// destination, credential, and host-key knobs. Every route's
    /// first step, so the knobs are threaded in exactly one place.
    async fn call(&self, command: &str) -> Result<RemoteCall, SandboxError> {
        RemoteCall::open(
            &self.host,
            self.private_key.as_deref(),
            self.known_hosts_path(),
            self.strict_host_keys(),
            self.config_path(),
            command,
        )
        .await
    }

    /// The known-hosts override, the tests' seam; outside the test
    /// feature the config's say rules.
    #[cfg(feature = "test-util")]
    fn known_hosts_path(&self) -> Option<&std::path::Path> {
        self.known_hosts.as_deref()
    }

    /// The strictness override, the tests' seam; outside the test
    /// feature the config's say rules.
    #[cfg(feature = "test-util")]
    fn strict_host_keys(&self) -> Option<bool> {
        self.strict
    }

    /// The config-file override, the tests' seam; outside the test
    /// feature the user's own config rules.
    #[cfg(feature = "test-util")]
    fn config_path(&self) -> Option<&std::path::Path> {
        self.ssh_config.as_deref()
    }

    /// The known-hosts override, always the config's say outside
    /// the test feature.
    #[cfg(not(feature = "test-util"))]
    fn known_hosts_path(&self) -> Option<&std::path::Path> {
        None
    }

    /// The strictness override, always the config's say outside the
    /// test feature.
    #[cfg(not(feature = "test-util"))]
    fn strict_host_keys(&self) -> Option<bool> {
        None
    }

    /// The config-file override, always the user's own outside the
    /// test feature.
    #[cfg(not(feature = "test-util"))]
    fn config_path(&self) -> Option<&std::path::Path> {
        None
    }
}

/// Reads one remote command's answer onto a sandbox result. A
/// nonzero exit failed in the remote command, with the stderr tail
/// as the detail; the could-not-reach case never gets here, the
/// connection refuses before any command runs.
fn remote_failure(code: Option<i32>, stderr: &str) -> SandboxError {
    let detail = stderr.trim();
    let detail = if detail.is_empty() {
        match code {
            Some(_) => "the remote command failed".to_owned(),
            None => "the remote command left no exit status".to_owned(),
        }
    } else {
        detail.to_owned()
    };
    SandboxError::Failed(detail)
}

/// The remote `cd` prefix for the sandbox cwd, quoted for the remote
/// shell, the shim's trick, since ssh has no `--workdir` to prefer.
/// `None` for the empty cwd and the root, which need no `cd`.
fn cd_prefix(cwd: &str) -> Option<String> {
    if cwd.is_empty() || cwd == "/" {
        return None;
    }
    Some(format!("cd {} && ", shquote(cwd)))
}

/// The exec command: `timeout --kill-after=5 <secs> bash -c
/// <script>`, the remote cwd handled by the `cd` prefix. The empty
/// cwd runs in the remote login's default directory. The script
/// rides as one quoted word: the far end reparses the command
/// string with the login shell, so an unquoted script would reach
/// `bash -c` as a word list.
#[cfg(test)]
fn exec_command(timeout_secs: u64, cwd: &str, command: &str) -> String {
    exec_command_env(
        timeout_secs,
        cwd,
        command,
        &std::collections::BTreeMap::new(),
    )
}

/// The env-carrying twin: `env 'K=V' ...` ahead of `timeout`, so the
/// login shell's `timeout` and the `bash` under it see the vars, the
/// kit's `BASH_ENV` included.
fn exec_command_env(
    timeout_secs: u64,
    cwd: &str,
    command: &str,
    env: &std::collections::BTreeMap<String, String>,
) -> String {
    let script = match cd_prefix(cwd) {
        Some(prefix) => format!("{prefix}{command}"),
        None => command.to_owned(),
    };
    let env_prefix = if env.is_empty() {
        String::new()
    } else {
        let words: Vec<String> = env
            .iter()
            .map(|(key, value)| shquote(&format!("{key}={value}")))
            .collect();
        format!("env {} ", words.join(" "))
    };
    format!(
        "{env_prefix}timeout --kill-after=5 {timeout_secs} bash -c {}",
        shquote(&script)
    )
}

/// The read command: `cat <path>`, the path quoted for the remote
/// shell.
fn read_command(path: &str) -> String {
    format!("cat {}", shquote(path))
}

/// The mkdir command for the write route's parent step:
/// `mkdir -p <parent>`.
fn mkdir_command(parent: &str) -> String {
    format!("mkdir -p {}", shquote(parent))
}

/// The write command: `tee <path>`, the bytes arriving on the
/// channel's stdin.
fn write_command(path: &str) -> String {
    format!("tee {}", shquote(path))
}

/// The git-seed command: `git clone <url> <workspace>`, the
/// repository becomes the remote workspace root. Both operands are
/// quoted for the remote shell.
fn clone_command(url: &str, workspace: &str) -> String {
    format!("git clone {} {}", shquote(url), shquote(workspace))
}

/// The tarball-seed command: `tar -xzf - -C <workspace>`, the
/// tarball streamed raw on the channel's stdin, no base64 hop,
/// binary-safe.
fn tar_command(workspace: &str) -> String {
    format!("tar -xzf - -C {}", shquote(workspace))
}

/// The parent directory of an absolute path, `None` at the root,
/// there is no directory above it to make.
fn parent_of(path: &str) -> Option<String> {
    let trimmed = path.trim_end_matches('/');
    trimmed.rsplit_once('/').and_then(|(parent, _)| {
        if parent.is_empty() {
            None
        } else {
            Some(parent.to_owned())
        }
    })
}

#[async_trait]
impl Sandbox for SshSandbox {
    async fn run(
        &self,
        request: RunRequest,
        cancelled: &AtomicBool,
        progress: &mut dyn ToolProgress,
    ) -> Result<RunOutcome, SandboxError> {
        // There is no local root to resolve against: the textual
        // guard is the whole defense on a remote path.
        check_remote_path(&request.cwd.to_string_lossy())?;
        // The remote `timeout` prefix is the budget's enforcer and
        // the run loop is the cancel; the sandbox cwd is remote and
        // rides in the script. The wall holds the budget plus the
        // kill grace, so a black-holed channel abandons itself with
        // the request's own timeout, not a pending forever.
        self.call(&exec_command_env(
            rhema_sandbox::exec_util::budget_secs(request.timeout),
            &request.cwd.to_string_lossy(),
            &request.command,
            &request.env,
        ))
        .await?
        .run(&request, cancelled, progress, request.timeout + EXEC_GRACE)
        .await
    }

    async fn seed(&self, seed: &WorkspaceSeed) -> Result<(), SandboxError> {
        match seed {
            WorkspaceSeed::GitClone(url) => {
                let (_, code, stderr) = self
                    .budgeted(SEED_BUDGET, async {
                        self.call(&clone_command(url, &self.workspace))
                            .await?
                            .capture()
                            .await
                    })
                    .await?;
                match code {
                    Some(0) => Ok(()),
                    _ => Err(remote_failure(code, &stderr)),
                }
            }
            WorkspaceSeed::Tarball { tar_gz_base64, .. } => {
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(tar_gz_base64)
                    .map_err(|err| {
                        SandboxError::Failed(format!("the seed tarball is not base64: {err}"))
                    })?;
                // `tar -C` needs its target to exist; `git clone`
                // makes its own, this one does not. One extra call
                // under the same budget.
                let (_, code, stderr) = self
                    .budgeted(SEED_BUDGET, async {
                        self.call(&mkdir_command(&self.workspace))
                            .await?
                            .capture()
                            .await
                    })
                    .await?;
                if code != Some(0) {
                    return Err(remote_failure(code, &stderr));
                }
                let (code, stderr) = self
                    .feed(SEED_BUDGET, &tar_command(&self.workspace), bytes)
                    .await?;
                match code {
                    Some(0) => Ok(()),
                    _ => Err(remote_failure(code, &stderr)),
                }
            }
        }
    }

    async fn read_file(&self, path: &str) -> Result<Vec<u8>, SandboxError> {
        check_remote_path(path)?;
        let (stdout, code, stderr) = self
            .budgeted(FS_BUDGET, async {
                self.call(&read_command(path)).await?.capture().await
            })
            .await?;
        match code {
            Some(0) => Ok(stdout),
            _ => Err(remote_failure(code, &stderr)),
        }
    }

    async fn write_file(&self, path: &str, contents: &[u8]) -> Result<(), SandboxError> {
        check_remote_path(path)?;
        // The shim's put creates the parent directories, and the
        // skills seeding writes two levels deep, one extra call
        // when there is a parent above the root to make.
        if let Some(parent) = parent_of(path) {
            let (_, code, stderr) = self
                .budgeted(FS_BUDGET, async {
                    self.call(&mkdir_command(&parent)).await?.capture().await
                })
                .await?;
            if code != Some(0) {
                return Err(remote_failure(code, &stderr));
            }
        }
        let (code, stderr) = self
            .feed(FS_BUDGET, &write_command(path), contents.to_vec())
            .await?;
        if code != Some(0) {
            return Err(remote_failure(code, &stderr));
        }
        Ok(())
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use base64::engine::general_purpose::STANDARD as BASE64;
    use rhema_sandbox::OutputOnly;
    use russh::server::{
        Auth, ChannelOpenHandle, Handler as ServerHandler, Msg, Server as ServerServer,
        Session as ServerSession, VerifiedPublicKey,
    };
    use russh::{Channel as ServerChannel, ChannelId};
    use std::collections::HashMap;
    use std::net::SocketAddr;
    use tempfile::TempDir;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio::sync::Mutex as AsyncMutex;

    use rhema_sandbox::exec_util::EXEC_LINE_BYTES;
    use session::{LineFeeds, Target, Verdict};
    use std::sync::Arc;
    // The handler trait must be in scope for `check_server_key`.
    use russh::client::Handler as _;

    /// A sandbox aimed at the in-process far end the tests spawn:
    /// one port per test so parallel tests never share a workspace,
    /// one `known_hosts` tempdir per test so first contact never
    /// learns into the runner's own file, and a config path that
    /// resolves to nothing so the resolution takes its defaults.
    /// The tempdir rides out of the call: the `known_hosts` file
    /// must outlive the sandbox.
    async fn sandbox() -> (SshSandbox, TempDir) {
        let port = spawn_testd().await;
        let known = tempfile::tempdir().expect("tempdir");
        let config = tempfile::tempdir().expect("tempdir");
        let sandbox = SshSandbox::new(format!("sandboxer@127.0.0.1:{port}"))
            .with_private_key(CLIENT_KEY)
            .with_ssh_config_path(config.path().join("no-such-config"))
            .with_known_hosts_path(known.path().join("known_hosts"))
            .with_workspace(format!("/tmp/rhema-ssh-test-{port}"));
        (sandbox, known)
    }

    // -- the destination -------------------------------------------------

    #[test]
    fn a_colon_that_names_a_port_splits_off() {
        assert_eq!(session::split_port("box:2200"), ("box", Some(2200)));
        assert_eq!(session::split_port("box"), ("box", None));
        // A scoped IPv6 literal has no port to split.
        assert_eq!(session::split_port("fe80::1%eth0"), ("fe80::1%eth0", None));
        assert_eq!(
            session::split_port("[2001:db8::1]:22"),
            ("2001:db8::1", Some(22))
        );
        // A bare unbracketed IPv6 literal is all host: more than one
        // colon and no brackets means no port to split, or its
        // numeric tail would parse as one.
        assert_eq!(session::split_port("fe80::1"), ("fe80::1", None));
        assert_eq!(session::split_port("2001:db8::1"), ("2001:db8::1", None));
        // A bracketed form with no port: the whole bracketed literal.
        assert_eq!(
            session::split_port("[2001:db8::1]"),
            ("[2001:db8::1]", None)
        );
    }

    #[test]
    fn the_destination_resolves_through_the_ssh_config() {
        let home = tempfile::tempdir().expect("tempdir");
        let config = home.path().join("config");
        std::fs::write(
            &config,
            "Host box\n  Hostname realbox.internal\n  User deployer\n  Port 2200\n",
        )
        .expect("the config is staged");
        // The alias resolves through the config's Host block.
        let target = Target::resolve("box", Some(&config)).expect("the alias resolves");
        assert_eq!(target.host, "realbox.internal");
        assert_eq!(target.user, "deployer");
        assert_eq!(target.port, 2200);
        // The explicit user@host:port pieces win over the config,
        // the way the ssh binary reads a destination.
        let target =
            Target::resolve("other@box:2345", Some(&config)).expect("the destination resolves");
        assert_eq!(target.host, "realbox.internal");
        assert_eq!(target.user, "other");
        assert_eq!(target.port, 2345);
    }

    #[test]
    fn a_destination_outside_the_config_takes_the_defaults() {
        let home = tempfile::tempdir().expect("tempdir");
        let config = home.path().join("config");
        std::fs::write(&config, "Host box\n  User deployer\n").expect("the config is staged");
        let target =
            Target::resolve("plain.example", Some(&config)).expect("the destination resolves");
        assert_eq!(target.host, "plain.example");
        assert_eq!(target.port, 22);
        // The user comes from the environment when nothing names it.
        assert!(!target.user.is_empty(), "a login is still wanted");
    }

    // -- the command strings ----------------------------------------------

    #[test]
    fn the_exec_command_prefixes_the_request_env() {
        let mut env = std::collections::BTreeMap::new();
        env.insert("BASH_ENV".to_owned(), "/x/init.sh".to_owned());
        let command = exec_command_env(30, "/src", "echo hi", &env);
        assert!(
            command.starts_with("env 'BASH_ENV=/x/init.sh' timeout --kill-after=5 30 bash -c "),
            "{command}"
        );
    }

    #[test]
    fn the_exec_command_quotes_the_script_as_one_word() {
        assert_eq!(
            exec_command(30, "/src", "echo hi"),
            "timeout --kill-after=5 30 bash -c 'cd '\\''/src'\\'' && echo hi'"
        );
    }

    #[test]
    fn a_sub_second_budget_never_reaches_a_timeout_zero() {
        // `timeout 0` is no timeout at all: the ceiled seconds are
        // computed before the line is shaped, so a 500 ms budget
        // prefixes `timeout 1`, never an ungated command.
        assert_eq!(
            rhema_sandbox::exec_util::budget_secs(Duration::from_millis(500)),
            1
        );
    }

    #[test]
    fn the_root_and_unset_cwd_need_no_cd() {
        for cwd in ["", "/"] {
            assert_eq!(
                exec_command(30, cwd, "echo hi"),
                "timeout --kill-after=5 30 bash -c 'echo hi'"
            );
        }
    }

    #[test]
    fn a_cwd_with_spaces_is_quoted_for_the_remote_shell() {
        assert!(
            exec_command(30, "/my dir", "echo hi").contains("'cd '\\''/my dir'\\'' && echo hi'")
        );
    }

    #[test]
    fn the_read_and_write_commands_quote_their_paths() {
        assert_eq!(read_command("/src/deep/note.md"), "cat '/src/deep/note.md'");
        assert_eq!(mkdir_command("/src/deep"), "mkdir -p '/src/deep'");
        assert_eq!(
            write_command("/src/deep/note.md"),
            "tee '/src/deep/note.md'"
        );
    }

    #[test]
    fn the_seed_commands_quote_url_and_destination() {
        assert_eq!(
            clone_command("https://example.com/repo", "/srv/workspace"),
            "git clone 'https://example.com/repo' '/srv/workspace'"
        );
        assert_eq!(
            tar_command("/srv/workspace"),
            "tar -xzf - -C '/srv/workspace'"
        );
    }

    #[test]
    fn the_parent_of_a_path_stops_at_the_root() {
        assert_eq!(parent_of("/src/deep/note.md").as_deref(), Some("/src/deep"));
        assert_eq!(parent_of("/note.md"), None);
        assert_eq!(parent_of("/"), None);
        assert_eq!(parent_of("/deep/"), None);
    }

    #[test]
    fn a_blank_private_key_keeps_the_hunt_on() {
        let sandbox = SshSandbox::new("box").with_private_key("   \n");
        assert!(sandbox.private_key.is_none());
        let sandbox = SshSandbox::new("box").with_private_key("  key text  ");
        assert_eq!(sandbox.private_key.as_deref(), Some("key text"));
    }

    // -- the host-key policy ----------------------------------------------

    /// The host-key decision against one `known_hosts` file: first
    /// contact learns, a stored key passes, a changed key refuses
    /// with the reason on the verdict.
    fn decide(
        known_hosts: &std::path::Path,
        strict: bool,
        key: &russh::keys::PublicKey,
    ) -> (bool, Option<String>) {
        let verdict = Verdict::default();
        let mut handler = session::HostKey {
            known_hosts: Some(known_hosts.to_path_buf()),
            target: ("127.0.0.1".to_owned(), 2222),
            strict,
            verdict: verdict.clone(),
        };
        let server_key = russh::keys::PublicKeyOrCertificate::PublicKey {
            key: key.clone(),
            hash_alg: None,
        };
        let answer = tokio::runtime::Runtime::new()
            .expect("runtime")
            .block_on(handler.check_server_key(&server_key))
            .expect("the check answers");
        (answer, verdict.reason())
    }

    #[test]
    fn first_contact_learns_the_host_key() {
        let home = tempfile::tempdir().expect("tempdir");
        let known_hosts = home.path().join("known_hosts");
        let key = russh::keys::PublicKey::from_openssh(HOST_PUB).expect("the host key parses");
        let (answer, reason) = decide(&known_hosts, false, &key);
        assert!(answer, "first contact learns: {reason:?}");
        assert!(reason.is_none());
        // The learned entry is on disk, and a second contact passes
        // without learning again.
        assert!(known_hosts.exists(), "the learned entry landed");
        let (answer, reason) = decide(&known_hosts, false, &key);
        assert!(answer, "the stored key passes: {reason:?}");
    }

    #[test]
    fn a_changed_host_key_refuses_with_the_line() {
        let home = tempfile::tempdir().expect("tempdir");
        let known_hosts = home.path().join("known_hosts");
        std::fs::write(&known_hosts, format!("[127.0.0.1]:2222 {OTHER_PUB}\n"))
            .expect("the other key is staged");
        let key = russh::keys::PublicKey::from_openssh(HOST_PUB).expect("the host key parses");
        let (answer, reason) = decide(&known_hosts, false, &key);
        assert!(!answer, "a changed key refuses");
        let reason = reason.expect("the refusal says why");
        assert!(reason.contains("disagrees"), "{reason}");
        assert!(reason.contains("line"), "{reason}");
    }

    #[test]
    fn strict_refuses_an_unknown_host_without_learning() {
        let home = tempfile::tempdir().expect("tempdir");
        let known_hosts = home.path().join("known_hosts");
        let key = russh::keys::PublicKey::from_openssh(HOST_PUB).expect("the host key parses");
        let (answer, reason) = decide(&known_hosts, true, &key);
        assert!(!answer, "strict refuses an unknown host");
        let reason = reason.expect("the refusal says why");
        assert!(reason.contains("StrictHostKeyChecking"), "{reason}");
        assert!(!known_hosts.exists(), "strict does not learn");
    }

    // -- the line pump ------------------------------------------------------

    #[test]
    fn complete_lines_stream_and_the_tail_keeps_the_end() {
        let mut feeds = LineFeeds::new();
        let mut lines = Vec::new();
        {
            let mut emit = |line: &str| {
                lines.push(line.to_owned());
                Ok(())
            };
            feeds.feed(false, b"hi\nhalf", &mut emit).expect("feeds");
        }
        // The complete line streamed while the half line is still
        // pending, waiting for its newline.
        assert_eq!(lines, vec!["hi".to_owned()]);
        {
            let mut emit = |line: &str| {
                lines.push(line.to_owned());
                Ok(())
            };
            let (stdout, _stderr) = feeds.finish(&mut emit).expect("finishes");
            // The tail keeps the stream's last bytes, the lines
            // already delivered included, the way the result
            // always frames a command's output.
            assert_eq!(stdout, b"hi\nhalf\n");
        }
        assert_eq!(lines.last().map(String::as_str), Some("half"));
    }

    #[test]
    fn a_line_past_the_cap_is_truncated_with_a_marker() {
        let mut feeds = LineFeeds::new();
        let mut lines = Vec::new();
        let long = vec![b'a'; EXEC_LINE_BYTES + 10];
        {
            let mut emit = |line: &str| {
                lines.push(line.to_owned());
                Ok(())
            };
            feeds.feed(false, &long, &mut emit).expect("feeds");
            feeds.feed(false, b"\n", &mut emit).expect("feeds");
        }
        assert_eq!(lines.len(), 2, "the line and the marker: {lines:?}");
        assert_eq!(lines[0].len(), EXEC_LINE_BYTES);
        assert!(lines[1].contains("10 bytes dropped"), "{}", lines[1]);
    }

    #[test]
    fn a_carriage_return_is_stripped_the_way_a_reader_does() {
        let mut feeds = LineFeeds::new();
        let mut lines = Vec::new();
        {
            let mut emit = |line: &str| {
                lines.push(line.to_owned());
                Ok(())
            };
            feeds.feed(false, b"win\r\n", &mut emit).expect("feeds");
        }
        assert_eq!(lines, vec!["win".to_owned()]);
    }

    #[test]
    fn the_streams_stay_apart() {
        let mut feeds = LineFeeds::new();
        let mut emit = |_: &str| Ok(());
        feeds.feed(false, b"out", &mut emit).expect("feeds");
        feeds.feed(true, b"err", &mut emit).expect("feeds");
        let (stdout, stderr) = feeds.finish(&mut emit).expect("finishes");
        assert_eq!(stdout, b"out\n");
        assert_eq!(stderr, b"err\n");
    }

    // -- the far end: an ssh server in this process ------------------------

    /// The stdin queues of the commands the far end runs, by
    /// channel. `tee` and `tar` read their input from the client;
    /// bytes are queued here and written by a task, so the far
    /// end's protocol loop never blocks on a full pipe (a real
    /// sshd's channel IO does not either), and the client's end of
    /// input arrives as `channel_eof`, which drops the queue and
    /// with it the pipe.
    type Stdins = Arc<AsyncMutex<HashMap<ChannelId, tokio::sync::mpsc::UnboundedSender<Vec<u8>>>>>;

    /// The far end the tests talk to: an ssh server that accepts
    /// any key and runs every exec's command string through the
    /// local `bash`, exactly what a real sshd does with the login
    /// shell. No sshd on the box, nothing outside the loopback.
    #[derive(Clone)]
    struct Testd {
        stdins: Stdins,
    }

    impl ServerServer for Testd {
        type Handler = Testd;

        fn new_client(&mut self, _: Option<SocketAddr>) -> Self::Handler {
            self.clone()
        }

        fn handle_session_error(&mut self, error: <Self::Handler as ServerHandler>::Error) {
            eprintln!("the test far end's session died: {error:?}");
        }
    }

    impl ServerHandler for Testd {
        type Error = russh::Error;

        async fn auth_publickey(
            &mut self,
            _: &str,
            _: VerifiedPublicKey<'_>,
        ) -> Result<Auth, Self::Error> {
            Ok(Auth::Accept)
        }

        async fn channel_open_session(
            &mut self,
            _: ServerChannel<Msg>,
            reply: ChannelOpenHandle,
            _: &mut ServerSession,
        ) -> Result<(), Self::Error> {
            reply.accept().await;
            Ok(())
        }

        async fn exec_request(
            &mut self,
            channel: ChannelId,
            data: &[u8],
            session: &mut ServerSession,
        ) -> Result<(), Self::Error> {
            let _ = session.channel_success(channel);
            let script = String::from_utf8_lossy(data).into_owned();
            let mut child = tokio::process::Command::new("bash")
                .arg("-c")
                .arg(script)
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .expect("the test far end spawns bash");
            let stdin = child.stdin.take().expect("stdin piped");
            let (queue, mut queued) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
            self.stdins.lock().await.insert(channel, queue);
            tokio::spawn(async move {
                let mut stdin = stdin;
                while let Some(bytes) = queued.recv().await {
                    if stdin.write_all(&bytes).await.is_err() {
                        break;
                    }
                }
                // The queue is gone: the command's stdin half goes,
                // and a `tee` or `tar` reading it sees the end.
            });
            let handle = session.handle();
            tokio::spawn(run_command_to_its_streams(
                channel,
                child,
                self.stdins.clone(),
                handle.clone(),
                handle,
            ));
            Ok(())
        }

        async fn data(
            &mut self,
            channel: ChannelId,
            data: &[u8],
            _: &mut ServerSession,
        ) -> Result<(), Self::Error> {
            if let Some(queue) = self.stdins.lock().await.get(&channel) {
                queue.send(data.to_vec()).ok();
            }
            Ok(())
        }

        async fn channel_eof(
            &mut self,
            channel: ChannelId,
            _: &mut ServerSession,
        ) -> Result<(), Self::Error> {
            // End of the client's input: the queue goes, and with
            // it the pipe the command reads.
            self.stdins.lock().await.remove(&channel);
            Ok(())
        }
    }

    /// Pumps one command's stdout and stderr onto its channel, then
    /// its exit status, then closes the channel. A write that fails
    /// (the client hung up: a cap dropped the connection mid-run)
    /// kills the command, there is nobody left to read it.
    async fn run_command_to_its_streams(
        channel: ChannelId,
        mut child: tokio::process::Child,
        stdins: Stdins,
        out_handle: russh::server::Handle,
        err_handle: russh::server::Handle,
    ) {
        let mut stdout = child.stdout.take().expect("stdout piped");
        let mut stderr = child.stderr.take().expect("stderr piped");
        let end_handle = err_handle.clone();
        let out_task = tokio::spawn(async move {
            let handle = out_handle;
            let mut buffer = vec![0u8; 64 * 1024];
            loop {
                let read = stdout.read(&mut buffer).await.unwrap_or(0);
                if read == 0 {
                    break;
                }
                if handle.data(channel, buffer[..read].to_vec()).await.is_err() {
                    return false;
                }
            }
            true
        });
        let err_task = tokio::spawn(async move {
            let handle = err_handle;
            let mut buffer = vec![0u8; 64 * 1024];
            loop {
                let read = stderr.read(&mut buffer).await.unwrap_or(0);
                if read == 0 {
                    break;
                }
                if handle
                    .extended_data(channel, 1, buffer[..read].to_vec())
                    .await
                    .is_err()
                {
                    return false;
                }
            }
            true
        });
        let out_lived = out_task.await.unwrap_or(false);
        let err_lived = err_task.await.unwrap_or(false);
        if !(out_lived && err_lived) {
            let _ = child.kill().await;
            stdins.lock().await.remove(&channel);
            return;
        }
        let code = child.wait().await.ok().and_then(|status| status.code());
        stdins.lock().await.remove(&channel);
        if let Some(code) = code {
            let _ = end_handle.exit_status_request(channel, code as u32).await;
        }
        let _ = end_handle.eof(channel).await;
        let _ = end_handle.close(channel).await;
    }

    /// Spawns the far end on a loopback port nobody else holds.
    async fn spawn_testd() -> u16 {
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("the loopback binds");
        let port = listener.local_addr().expect("the address").port();
        let config = Arc::new(russh::server::Config {
            keys: vec![russh::keys::decode_secret_key(HOST_KEY, None).expect("the host key")],
            auth_rejection_time: Duration::from_secs(1),
            auth_rejection_time_initial: Some(Duration::ZERO),
            ..Default::default()
        });
        let mut server = Testd {
            stdins: Arc::new(AsyncMutex::new(HashMap::new())),
        };
        tokio::spawn(async move {
            use russh::server::Server as _;
            let _ = server.run_on_socket(config, &listener).await;
        });
        port
    }

    /// One exec through the sandbox with nothing cancelled and
    /// nothing watching the progress.
    async fn run(sandbox: &SshSandbox, request: RunRequest) -> Result<RunOutcome, SandboxError> {
        sandbox
            .run(request, &AtomicBool::new(false), &mut OutputOnly(|_| {}))
            .await
    }

    // -- the routes against the in-process far end ------------------------

    #[tokio::test]
    async fn a_command_runs_remotely_with_its_lines_live() {
        let (sandbox, _known) = sandbox().await;
        let mut lines: Vec<String> = Vec::new();
        let outcome = sandbox
            .run(
                RunRequest {
                    command: "echo hi".into(),
                    cwd: "/".into(),
                    timeout: Duration::from_secs(30),

                    ..Default::default()
                },
                &AtomicBool::new(false),
                &mut OutputOnly(|line| lines.push(line.to_owned())),
            )
            .await
            .expect("the command runs remotely");
        assert_eq!(outcome.code, Some(0));
        assert_eq!(outcome.stdout, b"hi\n");
        assert_eq!(lines, vec!["hi\n".to_owned()]);
    }

    #[tokio::test]
    async fn a_live_command_past_its_budget_is_a_timeout() {
        let (sandbox, _known) = sandbox().await;
        let error = run(
            &sandbox,
            RunRequest {
                command: "sleep 5".into(),
                cwd: "/".into(),
                timeout: Duration::from_secs(1),

                ..Default::default()
            },
        )
        .await
        .expect_err("the budget kills the command");
        assert_eq!(error, SandboxError::Timeout(1));
    }

    // A channel that never answers owes the caller the request's own
    // timeout: the far end runs a command that writes nothing and
    // never exits, and the small named wall exercises the breach
    // without paying the grace. Dropping the call is the abandon, the
    // connection leaves with it.
    #[tokio::test]
    async fn a_transport_that_never_answers_breaches_as_the_budget() {
        let (sandbox, _known) = sandbox().await;
        let call = sandbox
            .call("sleep 300")
            .await
            .expect("the far end takes the call");
        let request = RunRequest {
            command: "sleep 300".into(),
            cwd: "/".into(),
            // The budget the model reads when the wall ends the run.
            timeout: Duration::from_secs(1),

            ..Default::default()
        };
        let started = std::time::Instant::now();
        let error = call
            .run(
                &request,
                &AtomicBool::new(false),
                &mut OutputOnly(|_| {}),
                Duration::from_millis(300),
            )
            .await
            .expect_err("the wall ends the run");
        assert_eq!(error, SandboxError::Timeout(1));
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "the wall did not end the run: {:?}",
            started.elapsed()
        );
    }

    #[tokio::test]
    async fn a_failed_command_rides_in_the_outcome() {
        let (sandbox, _known) = sandbox().await;
        // The exec route's nonzero exit is the command's own
        // result, not an error: only the `timeout` prefix's 124 and
        // 137 read as errors, and the model reads the code itself.
        let outcome = run(
            &sandbox,
            RunRequest {
                command: "echo broken >&2; exit 3".into(),
                cwd: "/".into(),
                timeout: Duration::from_secs(30),

                ..Default::default()
            },
        )
        .await
        .expect("the command's failure is its result");
        assert_eq!(outcome.code, Some(3));
        assert_eq!(outcome.stderr, b"broken\n");
    }

    #[tokio::test]
    async fn files_round_trip_through_the_remote() {
        let (sandbox, _known) = sandbox().await;
        // The fs ops take remote-absolute paths; the workspace root
        // is the seed's target, so the test's paths hang under it.
        let note = format!("{}/deep/note.md", sandbox.workspace);
        sandbox
            .write_file(&note, b"# note")
            .await
            .expect("the write lands remotely");
        let read = sandbox
            .read_file(&note)
            .await
            .expect("the read lands remotely");
        assert_eq!(read, b"# note");
        // The remote scratch directory is this test's own; its
        // removal is best-effort cleanup, not an assertion.
        let _ = run(
            &sandbox,
            RunRequest {
                command: format!("rm -rf {}", sandbox.workspace),
                cwd: "/".into(),
                timeout: Duration::from_secs(30),

                ..Default::default()
            },
        )
        .await;
    }

    #[tokio::test]
    async fn a_feed_is_not_blocked_by_a_flooding_stderr() {
        let (sandbox, _known) = sandbox().await;
        // The command floods its stderr (8 MiB, past any buffering)
        // before it reads the bytes on stdin: a writer that drains
        // stderr only after the feed would deadlock on both sides,
        // the trap the old full-pipe test caught on two OS pipes.
        let call = sandbox
            .call("head -c 8388608 /dev/zero >&2; cat > /dev/null")
            .await
            .expect("the far end answers");
        let (code, stderr) =
            tokio::time::timeout(Duration::from_secs(30), call.feed(vec![b'x'; 512 * 1024]))
                .await
                .expect("the feed finishes")
                .expect("the feed lands");
        assert_eq!(code, Some(0));
        assert_eq!(stderr.len(), 8_388_608);
    }

    #[tokio::test]
    async fn a_capture_past_its_cap_fails_in_band() {
        let (sandbox, _known) = sandbox().await;
        let error = sandbox
            .read_file("/dev/zero")
            .await
            .expect_err("an endless read fails in band");
        match error {
            SandboxError::Failed(detail) => assert!(detail.contains("cap"), "{detail}"),
            error => panic!("a plain failure was wanted, got {error:?}"),
        }
    }

    #[tokio::test]
    async fn seeding_clones_into_the_remote_workspace_root() {
        let (sandbox, _known) = sandbox().await;
        // A local-path repository: the far end runs on this machine,
        // so the fixture's path is as valid remotely as it is here.
        let repo = tempfile::tempdir().expect("tempdir");
        let git = |args: &[&str]| {
            let output = std::process::Command::new("git")
                .args(args)
                .current_dir(repo.path())
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_SYSTEM", "/dev/null")
                .output()
                .expect("git runs");
            assert!(
                output.status.success(),
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&output.stderr),
            );
        };
        git(&["init", "-q"]);
        std::fs::write(repo.path().join("seeded.txt"), b"from git")
            .expect("the fixture file is written");
        git(&["add", "."]);
        git(&[
            "-c",
            "user.email=test@example.com",
            "-c",
            "user.name=test",
            "commit",
            "-q",
            "-m",
            "seed",
        ]);
        let remote = format!("{}-seed", sandbox.workspace);
        let sandbox = sandbox.with_workspace(remote.clone());
        sandbox
            .seed(&WorkspaceSeed::GitClone(
                repo.path().to_string_lossy().into_owned(),
            ))
            .await
            .expect("the remote clone lands");
        let read = sandbox
            .read_file(&format!("{remote}/seeded.txt"))
            .await
            .expect("the cloned file reads back");
        assert_eq!(read, b"from git");
        let _ = run(
            &sandbox,
            RunRequest {
                command: format!("rm -rf {remote}"),
                cwd: "/".into(),
                timeout: Duration::from_secs(30),

                ..Default::default()
            },
        )
        .await;
    }

    #[tokio::test]
    async fn seeding_a_tarball_unpacks_into_the_remote_workspace_root() {
        let (sandbox, _known) = sandbox().await;
        let stage = tempfile::tempdir().expect("tempdir");
        std::fs::write(stage.path().join("packed.txt"), b"from tar")
            .expect("the fixture file is written");
        // The tarball: one tar invocation over the stage directory,
        // the same bytes the seed route carries raw.
        let tarball = stage.path().join("packed.tar.gz");
        let packed = std::process::Command::new("tar")
            .args([
                "-czf",
                tarball.to_str().expect("the path is utf-8"),
                "-C",
                stage.path().to_str().expect("the path is utf-8"),
                "packed.txt",
            ])
            .output()
            .expect("tar runs");
        assert!(
            packed.status.success(),
            "tar failed: {}",
            String::from_utf8_lossy(&packed.stderr)
        );
        let remote = format!("{}-tar", sandbox.workspace);
        let sandbox = sandbox.with_workspace(remote.clone());
        sandbox
            .seed(&WorkspaceSeed::Tarball {
                name: "packed.tar.gz".to_owned(),
                tar_gz_base64: BASE64.encode(std::fs::read(&tarball).expect("the tarball reads")),
            })
            .await
            .expect("the remote unpack lands");
        let read = sandbox
            .read_file(&format!("{remote}/packed.txt"))
            .await
            .expect("the unpacked file reads back");
        assert_eq!(read, b"from tar");
        let _ = run(
            &sandbox,
            RunRequest {
                command: format!("rm -rf {remote}"),
                cwd: "/".into(),
                timeout: Duration::from_secs(30),

                ..Default::default()
            },
        )
        .await;
    }

    #[tokio::test]
    async fn an_unreachable_far_end_is_unavailable() {
        // Nothing listens on port 1; the destination resolution runs
        // against an empty config so the answer is about the
        // connect, not about some real config's Host block.
        let home = tempfile::tempdir().expect("tempdir");
        let sandbox = SshSandbox::new("127.0.0.1:1")
            .with_private_key(CLIENT_KEY)
            .with_ssh_config_path(home.path().join("no-such-config"))
            .with_known_hosts_path(home.path().join("known_hosts"));
        let error = sandbox
            .read_file("/dev/null")
            .await
            .expect_err("nothing listens on port 1");
        assert!(matches!(error, SandboxError::Unavailable(_)), "{error:?}");
    }

    // -- the keys the tests authenticate with ------------------------------

    /// The client key, carried as text: this is the file-less path
    /// the tests exercise, the same one a claw's config would.
    const CLIENT_KEY: &str = "-----BEGIN OPENSSH PRIVATE KEY-----
b3BlbnNzaC1rZXktdjEAAAAABG5vbmUAAAAEbm9uZQAAAAAAAAABAAAAMwAAAAtzc2gtZW
QyNTUxOQAAACBxNpEzRKYi6QtAVVcXrpELZ0bHUICSAjA8nNF1Lf5M7QAAAJhlMwbAZTMG
wAAAAAtzc2gtZWQyNTUxOQAAACBxNpEzRKYi6QtAVVcXrpELZ0bHUICSAjA8nNF1Lf5M7Q
AAAED64HGuozufqmXFmm69w1RK3UqXgHAQCPndtT5mQKmFtnE2kTNEpiLpC0BVVxeukQtn
RsdQgJICMDyc0XUt/kztAAAAFHJoZW1hLXNzaC1jcmF0ZS10ZXN0AQ==
-----END OPENSSH PRIVATE KEY-----";

    /// The far end's host key, what first contact learns.
    const HOST_KEY: &str = "-----BEGIN OPENSSH PRIVATE KEY-----
b3BlbnNzaC1rZXktdjEAAAAABG5vbmUAAAAEbm9uZQAAAAAAAAABAAAAMwAAAAtzc2gtZW
QyNTUxOQAAACDQiwYCfKAJpyVGb2R77x5zVn5ITl51VIV78RE3iJ8wiQAAAKAShqTiEoak
4gAAAAtzc2gtZWQyNTUxOQAAACDQiwYCfKAJpyVGb2R77x5zVn5ITl51VIV78RE3iJ8wiQ
AAAEAzl6luI5GjHeFVxV/Vd8fyleON5UDB8mo4yKUFrezlBdCLBgJ8oAmnJUZvZHvvHnNW
fkhOXnVUhXvxETeInzCJAAAAGXJoZW1hLXNzaC1jcmF0ZS10ZXN0LWhvc3QBAgME
-----END OPENSSH PRIVATE KEY-----";

    /// The far end's public key, the `known_hosts` line's half.
    const HOST_PUB: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAINCLBgJ8oAmnJUZvZHvvHnNWfkhOXnVUhXvxETeInzCJ rhema-ssh-crate-test-host";

    /// A different key, what a changed host key looks like.
    const OTHER_PUB: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIHE2kTNEpiLpC0BVVxeukQtnRsdQgJICMDyc0XUt/kzt rhema-ssh-crate-test";
}
