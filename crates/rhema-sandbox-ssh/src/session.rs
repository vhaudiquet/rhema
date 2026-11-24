//! The connection half of the ssh backend: destination resolution,
//! authentication, the host-key policy, and one remote call.
//!
//! The `ssh` binary used to own three jobs this crate now carries:
//! the destination resolved against the user's own `~/.ssh/config`
//! (russh-config), the credential hunt (an explicit key, the agent,
//! then the default key files), and the host-key check against
//! `known_hosts`. The policy here is the daemon-friendly reading of
//! OpenSSH's defaults, `accept-new`: learn a host on first contact,
//! refuse a key that disagrees with a stored one, since a daemon
//! cannot hold the interactive "are you sure" prompt. The remote
//! command strings and the trait implementation stay in [`crate`];
//! this module only speaks the wire.
#![forbid(unsafe_code)]

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use russh::client::{self, Handle};
#[cfg(unix)]
use russh::keys::agent::client::AgentClient;
use russh::keys::known_hosts::{
    known_host_keys, known_host_keys_path, learn_known_hosts, learn_known_hosts_path,
};
use russh::keys::{PrivateKeyWithHashAlg, decode_secret_key, load_secret_key};
use russh::{Channel, ChannelMsg};

use rhema_sandbox::exec_util::{
    EXEC_CAPTURE_BYTES, EXEC_LINE_BYTES, EXEC_TAIL_BYTES, classify_exit,
};
use rhema_sandbox::{RunOutcome, RunRequest, SandboxError, ToolProgress};

/// How often the cancelled flag is sampled while the channel is
/// quiet, the same tick `exec_util`'s pump keeps. An `AtomicBool`
/// carries no wakeup, so a silent command would otherwise never
/// observe a flip; the budget belongs to the remote `timeout`
/// prefix, this only observes a cancel.
const CANCEL_TICK: Duration = Duration::from_millis(100);

/// The stderr extension number: ssh's second stream, where a remote
/// command's stderr rides (`EXTENDED_DATA`, `ext` 1 per RFC 4254).
const STDERR_EXT: u32 = 1;

/// The destination as the wire needs it: a login, a host, a port,
/// and what the user's own config advises about keys and host keys.
#[derive(Debug, Clone)]
pub(crate) struct Target {
    /// The remote login, `user@` on the destination or the config's
    /// `User`, else the local `USER`.
    pub(crate) user: String,
    /// The host the TCP connection opens to, the config's `Hostname`
    /// when the destination is an alias.
    pub(crate) host: String,
    /// The TCP port, the config's `Port` over 22.
    pub(crate) port: u16,
    /// The config's `IdentityFile`s, tried after the agent.
    pub(crate) identity_files: Vec<PathBuf>,
    /// The config's `UserKnownHostsFile`, else the default
    /// `~/.ssh/known_hosts`.
    pub(crate) known_hosts: Option<PathBuf>,
    /// The config's `StrictHostKeyChecking`, else `accept-new`.
    pub(crate) strict: bool,
}

impl Target {
    /// Resolves one destination: any alias or `user@host` the local
    /// config resolves, with an optional `user@` prefix and `:port`
    /// suffix. The explicit pieces win over the config, the config
    /// over the defaults, the way the `ssh` binary reads a
    /// destination. `ProxyJump`/`ProxyCommand` are not followed: the
    /// destination must be directly reachable, an honest limit while
    /// the mode's own story is "no daemon, no tunnel".
    ///
    /// `config` points the resolution at one config file (the
    /// tests' seam) instead of the user's own `~/.ssh/config`.
    pub(crate) fn resolve(
        destination: &str,
        config: Option<&std::path::Path>,
    ) -> Result<Self, SandboxError> {
        let (explicit_user, rest) = match destination.rsplit_once('@') {
            Some((user, rest)) if !user.is_empty() => (Some(user.to_owned()), rest),
            // A bare `@host`: the config or the environment still
            // names the login.
            Some(_) => (None, destination.trim_start_matches('@')),
            None => (None, destination),
        };
        let (alias, explicit_port) = split_port(rest);
        let loaded = match config {
            Some(path) => russh_config::parse_path(path, alias),
            None => russh_config::parse_home(alias),
        };
        let config = match loaded {
            Ok(config) => config,
            // No config file is the common case, the defaults rule.
            // A config that exists but does not read must not be
            // silently ignored: its User or IdentityFile was the
            // login's plan.
            Err(russh_config::Error::Io(err)) if err.kind() == std::io::ErrorKind::NotFound => {
                russh_config::Config::default(alias)
            }
            Err(err) => {
                return Err(SandboxError::Failed(format!(
                    "the ssh config does not read: {err}"
                )));
            }
        };
        let user = explicit_user
            .or_else(|| Some(config.user()))
            .filter(|user| !user.is_empty())
            .or_else(|| std::env::var("USER").ok().filter(|user| !user.is_empty()))
            .ok_or_else(|| {
                SandboxError::Failed(format!(
                    "the ssh sandbox has no login for {destination:?}: no user@ on the \
                     destination, no User in the ssh config, no USER in the environment"
                ))
            })?;
        Ok(Self {
            user,
            host: config.host().to_owned(),
            port: explicit_port.unwrap_or_else(|| config.port()),
            identity_files: config.host_config.identity_file.unwrap_or_default(),
            known_hosts: config.host_config.user_known_hosts_file.clone(),
            strict: config.host_config.strict_host_key_checking.unwrap_or(false),
        })
    }
}

/// Splits a trailing `:port` off a destination, bracketed IPv6
/// included. A colon that does not introduce a port stays part of
/// the name, the way a scoped IPv6 literal does; and more than one
/// colon with no brackets is a bare IPv6 literal (`fe80::1`), the
/// whole string the host and no port to split, or its tail would
/// parse (`fe80:` + `1`) and point the connection somewhere else
/// entirely.
pub(crate) fn split_port(rest: &str) -> (&str, Option<u16>) {
    if let Some((bracketed, port)) = rest.strip_prefix('[').and_then(|r| r.split_once("]:")) {
        if let Ok(port) = port.parse::<u16>() {
            return (bracketed, Some(port));
        }
        return (rest, None);
    }
    if rest.matches(':').count() > 1 {
        return (rest, None);
    }
    match rest.rsplit_once(':') {
        Some((host, port)) => match port.parse::<u16>() {
            Ok(port) => (host, Some(port)),
            // Not a port (an IPv6 tail): the whole rest is the host.
            Err(_) => (rest, None),
        },
        None => (rest, None),
    }
}

/// What the host-key check decided and, on a refusal, why. The
/// connect error on a refused key is russh's bare `UnknownKey`, so
/// the reason rides here for the caller to surface.
#[derive(Clone, Default)]
pub(crate) struct Verdict(Arc<Mutex<Option<String>>>);

impl Verdict {
    fn note(&self, reason: String) {
        let mut slot = self.0.lock().expect("the verdict mutex is whole");
        if slot.is_none() {
            // The first reason stands; a second callback pass adds
            // nothing the caller needs.
            *slot = Some(reason);
        }
    }

    /// The refusal reason, if the check stored one.
    pub(crate) fn reason(&self) -> Option<String> {
        self.0.lock().expect("the verdict mutex is whole").clone()
    }
}

/// The client handler: the one callback russh requires is the
/// server-key check, and this one is `accept-new`. First contact
/// learns the key into `known_hosts`; a key that disagrees with a
/// stored entry refuses, and so does an unknown key when the user's
/// config set `StrictHostKeyChecking yes`.
pub(crate) struct HostKey {
    /// Where the host keys live, `None` for the default file.
    pub(crate) known_hosts: Option<PathBuf>,
    /// The resolved host and port, what a `known_hosts` entry is
    /// keyed by.
    pub(crate) target: (String, u16),
    /// Refuse unknown hosts (the config's `StrictHostKeyChecking
    /// yes`) instead of learning on first contact.
    pub(crate) strict: bool,
    /// Where a refusal's reason lands for the caller.
    pub(crate) verdict: Verdict,
}

impl client::Handler for HostKey {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        server_key: &russh::keys::PublicKeyOrCertificate,
    ) -> Result<bool, Self::Error> {
        let russh::keys::PublicKeyOrCertificate::PublicKey { key, .. } = server_key else {
            // A certificate's trust rides its certificate authority,
            // which `known_hosts` records under `@cert-authority`
            // lines this check does not parse. Plain keys are the
            // sandbox's story; a far end offering a certificate is a
            // far end this backend did not plan for.
            self.verdict.note(
                "the far end offered a certificate, this backend checks plain keys only".into(),
            );
            return Ok(false);
        };
        // The entries recorded for this host and port, compared on
        // the key data and not on russh's whole-key equality, which
        // includes the comment: an entry learned from a key that
        // carried one would otherwise fail its own check.
        let recorded = match &self.known_hosts {
            Some(path) => known_host_keys_path(&self.target.0, self.target.1, path),
            None => known_host_keys(&self.target.0, self.target.1),
        };
        let mut known = false;
        let mut changed = None;
        match recorded {
            Ok(entries) => {
                for (line, recorded) in entries {
                    if recorded.algorithm() != key.algorithm() {
                        // A different key type is an unrelated
                        // entry, not a disagreement.
                        continue;
                    }
                    if recorded.key_data() == key.key_data() {
                        known = true;
                    } else if changed.is_none() {
                        changed = Some(line);
                    }
                }
            }
            Err(err) => {
                // A check that cannot run refuses: a sandbox that
                // cannot say whether the host is known must not
                // connect anyway.
                self.verdict
                    .note(format!("the known_hosts check failed: {err}"));
                return Ok(false);
            }
        }
        if known {
            return Ok(true);
        }
        if let Some(line) = changed {
            self.verdict.note(format!(
                "{host}:{port}'s key disagrees with the known_hosts entry at line {line}, refusing a changed host key",
                host = self.target.0,
                port = self.target.1,
            ));
            return Ok(false);
        }
        if self.strict {
            self.verdict.note(format!(
                "{host}:{port} is not in known_hosts and StrictHostKeyChecking is yes, \
                 refusing an unknown host",
                host = self.target.0,
                port = self.target.1,
            ));
            return Ok(false);
        }
        let learned = match &self.known_hosts {
            Some(path) => learn_known_hosts_path(&self.target.0, self.target.1, key, path),
            None => learn_known_hosts(&self.target.0, self.target.1, key),
        };
        match learned {
            Ok(()) => Ok(true),
            Err(err) => {
                self.verdict.note(format!(
                    "first contact with {host}:{port}, but the key could not be learned \
                     into known_hosts: {err}",
                    host = self.target.0,
                    port = self.target.1,
                ));
                Ok(false)
            }
        }
    }
}

/// One remote call: the connection and the channel a command runs
/// on. Dropping it drops the whole connection, which is the cancel
/// and the budget: the local `ssh` process this replaces died the
/// same way, and the remote command may finish without anyone
/// reading its output, ssh carries no cancel.
pub(crate) struct RemoteCall {
    /// The connection, kept alive for the channel's sake. The field
    /// is named for what dropping it does.
    #[allow(dead_code)]
    connection: Handle<HostKey>,
    /// The channel the command runs on, where its streams arrive.
    channel: Channel<client::Msg>,
}

impl RemoteCall {
    /// Connects, authenticates, opens a session channel, and starts
    /// `command` on it. The credential is the sandbox's explicit
    /// private key when it carries one, else the agent's identities,
    /// else the config's `IdentityFile`s and the default key files,
    /// in that order, the way the `ssh` binary hunts when it cannot
    /// prompt. Nothing here prompts: no reachable credential is an
    /// unavailable sandbox, not a conversation.
    pub(crate) async fn open(
        destination: &str,
        private_key: Option<&str>,
        known_hosts: Option<&std::path::Path>,
        strict: Option<bool>,
        ssh_config: Option<&std::path::Path>,
        command: &str,
    ) -> Result<Self, SandboxError> {
        let target = Target::resolve(destination, ssh_config)?;
        let verdict = Verdict::default();
        let config = Arc::new(client::Config::default());
        let handler = HostKey {
            known_hosts: known_hosts
                .map(PathBuf::from)
                .or_else(|| target.known_hosts.clone()),
            target: (target.host.clone(), target.port),
            strict: strict.unwrap_or(target.strict),
            verdict: verdict.clone(),
        };
        let mut connection =
            match client::connect(config, (target.host.as_str(), target.port), handler).await {
                Ok(connection) => connection,
                Err(err) => {
                    let detail = verdict.reason().unwrap_or_else(|| err.to_string());
                    return Err(SandboxError::Unavailable(format!(
                        "the ssh sandbox cannot reach {host}:{port}: {detail}",
                        host = target.host,
                        port = target.port,
                    )));
                }
            };
        authenticate(&mut connection, &target, private_key).await?;
        let channel = connection.channel_open_session().await.map_err(|err| {
            SandboxError::Unavailable(format!("the far end refused a channel: {err}"))
        })?;
        channel
            .exec(true, command.to_owned())
            .await
            .map_err(|err| {
                SandboxError::Unavailable(format!("the far end refused the exec: {err}"))
            })?;
        Ok(Self {
            connection,
            channel,
        })
    }

    /// Drives the command to completion with its streams pumped:
    /// complete lines live through `progress` as they arrive, each
    /// stream keeps a capped tail, a cancel is observed before the
    /// start, between lines, and while the channel is quiet, and the
    /// exit read goes through the `timeout` prefix's codes. The
    /// message loop is the shape the old child pump had, over one
    /// multiplexed stream instead of two pipes.
    ///
    /// `wall` is the host-side deadline: the remote `timeout` prefix
    /// owns the command's budget and wins first, and past the wall a
    /// channel that never answers is abandoned, returning drops the
    /// connection and with it the channel, the far end's command
    /// losing its reader. The report is the request's own budget,
    /// the same shape the prefix's 124 reads as, whichever side
    /// drew the line.
    pub(crate) async fn run(
        mut self,
        request: &RunRequest,
        cancelled: &AtomicBool,
        progress: &mut dyn ToolProgress,
        wall: Duration,
    ) -> Result<RunOutcome, SandboxError> {
        if cancelled.load(Ordering::Relaxed) {
            return Err(SandboxError::Failed("cancelled".into()));
        }
        let deadline = tokio::time::Instant::now() + wall;
        let mut streams = LineFeeds::new();
        let mut code = None;
        loop {
            let message = tokio::select! {
                message = self.channel.wait() => message,
                // The wall: the prefix had its budget and the
                // channel never answered. Dropping `self` (the
                // connection's handle included) abandons the channel.
                _ = tokio::time::sleep_until(deadline) => {
                    return Err(SandboxError::Timeout(rhema_sandbox::exec_util::budget_secs(request.timeout)));
                }
                _ = tokio::time::sleep(CANCEL_TICK) => {
                    if cancelled.load(Ordering::Relaxed) {
                        return Err(SandboxError::Failed("cancelled".into()));
                    }
                    continue;
                }
            };
            let Some(message) = message else { break };
            match message {
                ChannelMsg::Data { data } => streams.feed(false, &data, &mut |line| {
                    progress.output(&format!("{line}\n"));
                    if cancelled.load(Ordering::Relaxed) {
                        return Err(SandboxError::Failed("cancelled".into()));
                    }
                    Ok(())
                })?,
                ChannelMsg::ExtendedData { data, ext } if ext == STDERR_EXT => {
                    streams.feed(true, &data, &mut |_| Ok(()))?;
                }
                ChannelMsg::ExitStatus { exit_status } => code = Some(exit_status as i32),
                // Eof and Close carry no bytes: the loop ends at
                // wait()'s None, when the channel shuts for good.
                _ => {}
            }
        }
        // The command is done: the model sees its true result
        // whatever the turn decided meanwhile, so the flag is not
        // rechecked here.
        let (stdout, stderr) = streams.finish(&mut |line| {
            progress.output(&format!("{line}\n"));
            Ok(())
        })?;
        let code = classify_exit(code, rhema_sandbox::exec_util::budget_secs(request.timeout))?;
        Ok(RunOutcome {
            stdout,
            stderr,
            code,
        })
    }

    /// Drives the command to completion reading its stdout under the
    /// capture cap, answering the bytes, the exit code, and the
    /// lossily-decoded stderr. A read of an endless file through
    /// `cat` must fail in band, not pin memory until the budget
    /// drops the call.
    pub(crate) async fn capture(mut self) -> Result<(Vec<u8>, Option<i32>, String), SandboxError> {
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut code = None;
        while let Some(message) = self.channel.wait().await {
            match message {
                ChannelMsg::Data { data } => {
                    if stdout.len() + data.len() > EXEC_CAPTURE_BYTES {
                        return Err(SandboxError::Failed(format!(
                            "the capture is past its {}-byte cap",
                            EXEC_CAPTURE_BYTES
                        )));
                    }
                    stdout.extend_from_slice(&data);
                }
                ChannelMsg::ExtendedData { data, ext } if ext == STDERR_EXT => {
                    stderr.extend_from_slice(&data);
                }
                ChannelMsg::ExitStatus { exit_status } => code = Some(exit_status as i32),
                _ => {}
            }
        }
        Ok((stdout, code, String::from_utf8_lossy(&stderr).into_owned()))
    }

    /// Feeds `bytes` to the command's stdin, signals end of input,
    /// and answers the exit code with the drained stderr. The write
    /// and the reads run side by side: a remote `tee` echoes the
    /// feed back on stdout, and a far end that fills its window
    /// while the feed is still crossing must not deadlock the
    /// writer, the way a full pipe pair once did.
    pub(crate) async fn feed(
        mut self,
        bytes: Vec<u8>,
    ) -> Result<(Option<i32>, String), SandboxError> {
        let mut writer = self.channel.make_writer();
        let mut feed = Box::pin(async move {
            use tokio::io::AsyncWriteExt;
            writer.write_all(&bytes).await?;
            writer.flush().await?;
            writer.shutdown().await?;
            Ok::<(), SandboxError>(())
        });
        let mut fed = false;
        let mut stderr = Vec::new();
        let mut code = None;
        loop {
            tokio::select! {
                // Biased toward the reads: the write only finishes
                // while the far end keeps consuming, so the reads
                // must not starve behind it.
                message = self.channel.wait() => {
                    let Some(message) = message else { break };
                    match message {
                        ChannelMsg::Data { .. } => {}
                        ChannelMsg::ExtendedData { data, ext } if ext == STDERR_EXT => {
                            stderr.extend_from_slice(&data);
                        }
                        ChannelMsg::ExitStatus { exit_status } => code = Some(exit_status as i32),
                        _ => {}
                    }
                }
                answer = feed.as_mut(), if !fed => {
                    fed = true;
                    answer.map_err(|err| {
                        SandboxError::Unavailable(format!("the feed stalled: {err}"))
                    })?;
                    self.channel.eof().await.map_err(|err| {
                        SandboxError::Unavailable(format!(
                            "the far end refused end of input: {err}"
                        ))
                    })?;
                }
            }
        }
        Ok((code, String::from_utf8_lossy(&stderr).into_owned()))
    }
}

/// Authenticates one open connection, in the hunt's order: the
/// sandbox's explicit key, the agent's identities, the config's
/// `IdentityFile`s, the default key files. The first method the far
/// end accepts wins; none accepting is an unavailable sandbox.
async fn authenticate(
    connection: &mut Handle<HostKey>,
    target: &Target,
    private_key: Option<&str>,
) -> Result<(), SandboxError> {
    // The hash only matters to RSA keys; the far end's preferences
    // pick it the way the `ssh` binary would.
    let hash = connection
        .best_supported_rsa_hash()
        .await
        .map_err(auth_error)?
        .flatten();
    if let Some(key_text) = private_key {
        let key = decode_secret_key(key_text, None).map_err(|err| {
            SandboxError::Unavailable(format!("the stored private key does not decode: {err}"))
        })?;
        let answer = connection
            .authenticate_publickey(
                target.user.clone(),
                PrivateKeyWithHashAlg::new(Arc::new(key), hash),
            )
            .await
            .map_err(auth_error)?;
        if answer.success() {
            return Ok(());
        }
        return Err(SandboxError::Unavailable(format!(
            "the far end refused the stored key for {}",
            target.user
        )));
    }
    // The agent client rides a unix socket; windows hunts the
    // stored and default keys only.
    #[cfg(unix)]
    let mut authed = agent_identities(connection, target, hash).await;
    #[cfg(not(unix))]
    let mut authed = false;
    if !authed {
        for path in key_file_candidates(&target.identity_files) {
            // A missing file or one that needs a passphrase is
            // skipped: the hunt cannot prompt.
            if let Ok(key) = load_secret_key(&path, None) {
                let answer = connection
                    .authenticate_publickey(
                        target.user.clone(),
                        PrivateKeyWithHashAlg::new(Arc::new(key), hash),
                    )
                    .await
                    .map_err(auth_error)?;
                if answer.success() {
                    authed = true;
                    break;
                }
            }
        }
    }
    if !authed {
        return Err(SandboxError::Unavailable(format!(
            "no credential the far end accepts for {}: no stored key, no agent \
             identities, no readable default key file",
            target.user
        )));
    }
    Ok(())
}

/// The agent identities, when `SSH_AUTH_SOCK` names a listening
/// agent: each identity gets one public-key attempt, the first the
/// far end accepts wins. A dead agent or a rejected identity is
/// skipped, the key-file hunt follows.
#[cfg(unix)]
async fn agent_identities(
    connection: &mut Handle<HostKey>,
    target: &Target,
    hash: Option<russh::keys::HashAlg>,
) -> bool {
    let Ok(mut agent) = AgentClient::connect_env().await else {
        return false;
    };
    let identities = agent.request_identities().await.unwrap_or_default();
    for identity in identities {
        let answer = connection
            .authenticate_publickey_with(
                target.user.clone(),
                identity.public_key().into_owned(),
                hash,
                &mut agent,
            )
            .await;
        if matches!(answer, Ok(result) if result.success()) {
            return true;
        }
    }
    false
}

/// The key files the hunt tries when no agent identity answers: the
/// config's `IdentityFile`s first, then OpenSSH's default names, in
/// its order.
fn key_file_candidates(from_config: &[PathBuf]) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = from_config.to_vec();
    if let Some(home) = std::env::var_os("HOME") {
        let home = PathBuf::from(home).join(".ssh");
        for name in ["id_ed25519", "id_ecdsa", "id_rsa"] {
            files.push(home.join(name));
        }
    }
    files
}

/// Maps a wire-level failure onto the unavailable error.
fn auth_error(err: russh::Error) -> SandboxError {
    SandboxError::Unavailable(format!("the ssh session failed: {err}"))
}

/// Per-stream line assembly for the run loop: complete lines out of
/// byte chunks that arrive in wire-sized pieces, with the cap, the
/// truncation marker, and the capped tail the `exec_util` pump
/// keeps, so a command's output reads the same whichever transport
/// carried it.
pub(crate) struct LineFeeds {
    stdout: StreamLines,
    stderr: StreamLines,
}

/// One stream's pending line bytes, its dropped-byte count for the
/// line being assembled, and its capped tail.
struct StreamLines {
    pending: Vec<u8>,
    dropped: u64,
    tail: Vec<u8>,
}

impl LineFeeds {
    pub(crate) fn new() -> Self {
        Self {
            stdout: StreamLines::new(),
            stderr: StreamLines::new(),
        }
    }

    /// Folds `chunk` into one stream, hands every complete line to
    /// `emit`, and keeps the tail. A line past [`EXEC_LINE_BYTES`]
    /// keeps its first bytes, counts the rest, and says so in a
    /// marker line, the way the pump does.
    pub(crate) fn feed(
        &mut self,
        is_stderr: bool,
        chunk: &[u8],
        emit: &mut dyn FnMut(&str) -> Result<(), SandboxError>,
    ) -> Result<(), SandboxError> {
        let stream = if is_stderr {
            &mut self.stderr
        } else {
            &mut self.stdout
        };
        stream.fold(chunk, emit)
    }

    /// Delivers each stream's pending bytes, a final line without
    /// its newline, and answers the capped tails.
    pub(crate) fn finish(
        mut self,
        emit: &mut dyn FnMut(&str) -> Result<(), SandboxError>,
    ) -> Result<(Vec<u8>, Vec<u8>), SandboxError> {
        self.stdout.drain_pending(emit)?;
        self.stderr.drain_pending(emit)?;
        Ok((self.stdout.tail, self.stderr.tail))
    }
}

impl StreamLines {
    fn new() -> Self {
        Self {
            pending: Vec::new(),
            dropped: 0,
            tail: Vec::new(),
        }
    }

    /// Walks `chunk`, extending the pending line up to the cap and
    /// counting the overrun, delivering at every newline.
    fn fold(
        &mut self,
        chunk: &[u8],
        emit: &mut dyn FnMut(&str) -> Result<(), SandboxError>,
    ) -> Result<(), SandboxError> {
        let mut consumed = 0;
        loop {
            let available = &chunk[consumed..];
            let newline = available.iter().position(|&byte| byte == b'\n');
            let length = newline.unwrap_or(available.len());
            let room = EXEC_LINE_BYTES.saturating_sub(self.pending.len());
            let take = length.min(room);
            self.pending.extend_from_slice(&available[..take]);
            self.dropped += (length - take) as u64;
            match newline {
                Some(at) => {
                    self.deliver(emit)?;
                    self.pending.clear();
                    self.dropped = 0;
                    consumed += at + 1;
                }
                None => break,
            }
        }
        Ok(())
    }

    /// Delivers the pending bytes as one line: lossily decoded, the
    /// carriage return one reader strips gone, capped at the front
    /// with the dropped count made visible in a marker line.
    fn deliver(
        &mut self,
        emit: &mut dyn FnMut(&str) -> Result<(), SandboxError>,
    ) -> Result<(), SandboxError> {
        let lossy = String::from_utf8_lossy(&self.pending).into_owned();
        let text = lossy.strip_suffix('\r').unwrap_or(&lossy);
        emit(text)?;
        self.append_capped(text.as_bytes());
        self.append_capped(b"\n");
        if self.dropped > 0 {
            // The overrun bytes were consumed but not kept: the
            // stream stays framed and the elision is visible.
            let marker = format!("[line truncated, {} bytes dropped]", self.dropped);
            emit(&marker)?;
            self.append_capped(marker.as_bytes());
            self.append_capped(b"\n");
        }
        Ok(())
    }

    /// A stream's end: pending bytes are a final line without its
    /// newline, delivered the same way. Nothing pending is nothing
    /// delivered.
    fn drain_pending(
        &mut self,
        emit: &mut dyn FnMut(&str) -> Result<(), SandboxError>,
    ) -> Result<(), SandboxError> {
        if !self.pending.is_empty() || self.dropped > 0 {
            self.deliver(emit)?;
            self.pending.clear();
            self.dropped = 0;
        }
        Ok(())
    }

    /// The tail keeps at most the last [`EXEC_TAIL_BYTES`], applied
    /// as the bytes land, so a runaway command never holds more.
    fn append_capped(&mut self, bytes: &[u8]) {
        self.tail.extend_from_slice(bytes);
        if self.tail.len() > EXEC_TAIL_BYTES {
            let excess = self.tail.len() - EXEC_TAIL_BYTES;
            self.tail.drain(..excess);
        }
    }
}
