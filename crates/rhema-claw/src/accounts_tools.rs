//! The identity tools: `identity_save`, `identity_list`,
//! `identity_forget`, and `identity_exec`, the model's surface over the
//! [`IdentityStore`].
//!
//! These are the one place the use-never-read contract could break, so
//! the answers are built to keep it: a save answers with the redacted
//! [`Account`] and the watch verdict — never the secret, which nothing
//! here re-renders; a list answers with the store's own redacted
//! records; a forget answers with what went. The store's `secret()`
//! has exactly one tool-side consumer, [`IDENTITY_EXEC`]: the credential
//! is injected into one child process's environment — the sanctioned
//! *destination* — and only that command's output returns, so the value
//! reaches its work without ever entering the conversation.
//!
//! These tools join the interactive chain only. A forge dispatch — whose
//! prompt is text a stranger wrote — never resolves them, so an injected
//! comment cannot ask its way to the accounts or act with their
//! credentials.
//!
//! The three store tools' specs live in `TOOL.yaml`s (one folder under
//! `tools/`); the code here is the operation half only. identity_exec
//! is its own [`ToolResolver`] and carries its spec inline — the one
//! deliberate exception, since its registry shape is the exec seam
//! itself.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use rhema_autonomous_agent::{ToolOp, ToolRegistry, async_trait};
use rhema_core::{ToolCall, ToolContext, ToolResolver, ToolResult, ToolSpec};
use rhema_identity::{Account, AccountDraft, IdentityStore, Secret, SecretKind};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::config::ForgeKind;
use crate::watch_store::ClawStore;

/// `identity_save`: store an account (label, service, url, username, and
/// the secret — required) and optionally register its forge watch.
struct IdentitySave {
    accounts: Arc<dyn IdentityStore>,
    watches: Arc<ClawStore>,
}

/// The save's arguments. `secret_kind` defaults to a token; a password
/// is stored but is never model-accessible or pollable in v1.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SaveArgs {
    label: String,
    service: String,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    username: Option<String>,
    secret: String,
    #[serde(default)]
    secret_kind: Option<String>,
    #[serde(default)]
    watch: bool,
}

/// What saving an account earns: the redacted account, whether the
/// service resolved to a forge the daemon watches, and whether the watch
/// got registered.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct SavedAccount {
    /// The stored account, redacted.
    pub account: Account,
    /// Whether `service`/`url` resolved to a watchable forge.
    pub forge_known: bool,
    /// Whether the forge watch got registered (asked for, forge known,
    /// and not already watched).
    pub watch_added: bool,
}

/// The forge an account's `service` (then its `url` host) names, when it
/// is one the daemon watches. A self-hosted instance is only recognizable
/// through an explicit service name — a URL alone cannot distinguish a
/// Gitea from a GitLab.
pub fn forge_of(service: &str, url: Option<&str>) -> Option<ForgeKind> {
    // Case-insensitive on the service: a user says "GitHub", the store
    // normalizes to "github", and both should name the forge.
    if let Some(kind) = ForgeKind::parse(service.trim().to_lowercase().as_str()) {
        return Some(kind);
    }
    let url = url?;
    let host = url
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(url)
        .split('/')
        .next()?;
    match host.trim_end_matches('/') {
        "github.com" => Some(ForgeKind::GitHub),
        "gitlab.com" => Some(ForgeKind::GitLab),
        _ => None,
    }
}

/// Parses the optional `secret_kind` spelling; `None` (absent) means a
/// token. An unknown spelling is an in-band error. Shared with the HTTP
/// route, which validates the same shape.
pub fn parse_kind(kind: Option<&str>) -> Result<SecretKind, String> {
    match kind.map(str::trim).filter(|name| !name.is_empty()) {
        None => Ok(SecretKind::Token),
        Some("token") => Ok(SecretKind::Token),
        Some("password") => Ok(SecretKind::Password),
        Some(other) => Err(format!(
            "unknown secret_kind {other:?}: a kind is 'token' or 'password'"
        )),
    }
}

/// Stores an account and, when asked and the service names a watchable
/// forge, registers its account watch — the explicit-at-creation sync:
/// no background reconciliation, so this is the one moment a watch
/// appears. The store's error and the watch verdict both land in-band.
pub fn save_account(
    accounts: &dyn IdentityStore,
    watches: &ClawStore,
    draft: AccountDraft,
    watch: bool,
) -> Result<SavedAccount, String> {
    let account = accounts.put(draft).map_err(|err| err.to_string())?;
    let forge = forge_of(&account.service, account.url.as_deref());
    let mut saved = SavedAccount {
        account,
        forge_known: forge.is_some(),
        watch_added: false,
    };
    if watch {
        if let Some(kind) = forge {
            let host = saved.account.url.as_deref().filter(|url| !url.is_empty());
            saved.watch_added = watches
                .add_account_watch(kind.name(), host, &saved.account.id)
                .map_err(|err| err.to_string())?;
        }
        // Asked to watch a service the daemon cannot watch: the save
        // stands, the watch verdict says why not.
    }
    Ok(saved)
}

/// Forgets an account and cascades its derived account watches — the
/// other half of the explicit sync. Returns whether the account existed
/// and how many watches went with it.
pub fn forget_account(
    accounts: &dyn IdentityStore,
    watches: &ClawStore,
    id: &str,
) -> Result<(bool, usize), String> {
    let forgot = accounts.forget(id).map_err(|err| err.to_string())?;
    let removed = if forgot {
        watches
            .remove_account_watches_for(id)
            .map_err(|err| err.to_string())?
    } else {
        0
    };
    Ok((forgot, removed))
}

#[async_trait]
impl ToolOp for IdentitySave {
    async fn invoke(&self, args: Value) -> Result<String, String> {
        let args: SaveArgs =
            serde_json::from_value(args).map_err(|err| format!("bad arguments: {err}"))?;
        let kind = parse_kind(args.secret_kind.as_deref())?;
        let mut draft = AccountDraft::new(args.label, args.service, Secret::new(kind, args.secret));
        if let Some(url) = args.url {
            draft = draft.with_url(url);
        }
        if let Some(username) = args.username {
            draft = draft.with_username(username);
        }
        let saved = save_account(
            self.accounts.as_ref(),
            self.watches.as_ref(),
            draft,
            args.watch,
        )?;
        Ok(json!({
            "saved": true,
            "id": saved.account.id,
            "label": saved.account.label,
            "service": saved.account.service,
            "secret_kind": saved.account.secret_kind,
            "watched": saved.watch_added,
            "watchable_forge": saved.forge_known,
        })
        .to_string())
    }
}

/// `identity_list`: every account, redacted — the store's own records,
/// which have no secret field to leak.
struct IdentityList {
    accounts: Arc<dyn IdentityStore>,
}

#[async_trait]
impl ToolOp for IdentityList {
    async fn invoke(&self, _args: Value) -> Result<String, String> {
        let accounts = self.accounts.list().map_err(|err| err.to_string())?;
        Ok(json!({ "accounts": accounts }).to_string())
    }
}

/// `identity_forget`: remove an account, its secret with it, and the
/// account watches derived from it.
struct IdentityForget {
    accounts: Arc<dyn IdentityStore>,
    watches: Arc<ClawStore>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ForgetArgs {
    id: String,
}

#[async_trait]
impl ToolOp for IdentityForget {
    async fn invoke(&self, args: Value) -> Result<String, String> {
        let args: ForgetArgs =
            serde_json::from_value(args).map_err(|err| format!("bad arguments: {err}"))?;
        let (forgot, watches_removed) =
            forget_account(self.accounts.as_ref(), self.watches.as_ref(), &args.id)?;
        Ok(json!({
            "forgot": forgot,
            "watches_removed": watches_removed,
        })
        .to_string())
    }
}

/// The compiled operations behind the manifests, by `run.builtin` id.
fn builtins(
    accounts: Arc<dyn IdentityStore>,
    watches: Arc<ClawStore>,
) -> BTreeMap<String, Arc<dyn ToolOp>> {
    BTreeMap::from([
        (
            "identity.forget".to_owned(),
            Arc::new(IdentityForget {
                accounts: accounts.clone(),
                watches: watches.clone(),
            }) as Arc<dyn ToolOp>,
        ),
        (
            "identity.list".to_owned(),
            Arc::new(IdentityList {
                accounts: accounts.clone(),
            }) as Arc<dyn ToolOp>,
        ),
        (
            "identity.save".to_owned(),
            Arc::new(IdentitySave { accounts, watches }) as Arc<dyn ToolOp>,
        ),
    ])
}

/// The store tools' `TOOL.yaml`s, embedded so the daemon ships them
/// with no filesystem layout: one folder per tool, named by tool, in
/// the order the registry serves them.
pub const MANIFESTS: &[(&str, &str)] = &[
    (
        "identity_forget",
        include_str!("../tools/identity_forget/TOOL.yaml"),
    ),
    (
        "identity_list",
        include_str!("../tools/identity_list/TOOL.yaml"),
    ),
    (
        "identity_save",
        include_str!("../tools/identity_save/TOOL.yaml"),
    ),
];

/// The identity tool set over the account store and the watch table —
/// specs from the embedded manifests, operations compiled here.
pub fn registry(accounts: Arc<dyn IdentityStore>, watches: Arc<ClawStore>) -> ToolRegistry {
    let (tools, skipped) =
        rhema_autonomous_agent::manifest::builtin_tools(MANIFESTS, &builtins(accounts, watches));
    assert!(
        skipped.is_empty(),
        "the identity tool manifests must all load: {skipped:?}"
    );
    let mut registry = ToolRegistry::new();
    for tool in tools {
        registry.register(tool);
    }
    registry
}

/// The tool's name on the wire.
pub const IDENTITY_EXEC: &str = "identity_exec";

/// How long one command may run before it is killed.
const EXEC_TIMEOUT: Duration = Duration::from_secs(180);

/// The longest a caller may stretch a command's budget.
const EXEC_TIMEOUT_MAX: Duration = Duration::from_secs(600);

/// The output tail a result carries, mirroring the bash tool's caps:
/// past either the byte or the line limit the head is elided and the
/// tail kept — a failure's summary lives at the end.
fn render_truncated(bytes: &[u8]) -> String {
    const LIMIT: usize = 50 * 1024;
    const LINES: usize = 2000;
    let text = String::from_utf8_lossy(bytes);
    let mut lines: Vec<&str> = text.lines().collect();
    if bytes.len() <= LIMIT && lines.len() <= LINES {
        return text.into_owned();
    }
    if lines.len() > LINES {
        lines = lines[lines.len() - LINES..].to_vec();
    }
    let mut kept = String::new();
    for line in lines {
        if kept.len() + line.len() + 1 > LIMIT {
            break;
        }
        kept.push_str(line);
        kept.push('\n');
    }
    let elided = bytes.len() - kept.len();
    format!("[output truncated — {elided} bytes elided]\n{kept}")
}

/// The service's own env names a token answers as, beyond the generic
/// `ACCOUNT_*` set: the names its CLI and API clients read.
fn service_env_names(service: &str) -> &'static [&'static str] {
    match service.trim().to_ascii_lowercase().as_str() {
        "github" => &["GITHUB_TOKEN", "GH_TOKEN"],
        "gitea" => &["GITEA_TOKEN"],
        "gitlab" => &["GITLAB_TOKEN"],
        _ => &[],
    }
}

/// The git credential helper the command runs with: a shell snippet
/// that answers git's https prompts from the injected environment. The
/// secret lives only in the child process's env — never in the value
/// itself, never in the output.
const GIT_CREDENTIAL_HELPER: &str = "!f() { \
    printf 'username=%s\\n' \"$ACCOUNT_USERNAME\"; \
    printf 'password=%s\\n' \"$ACCOUNT_SECRET\"; }; f";

/// `identity_exec`: act with a stored account. The one tool-side
/// consumer of the store's `secret()` — the credential is injected into
/// one child process's environment and a git credential helper, the
/// command runs, and only its output returns. The value reaches its
/// destination; it never reaches the conversation.
pub struct IdentityExec {
    accounts: Arc<dyn IdentityStore>,
    workspace: PathBuf,
}

impl IdentityExec {
    /// The executor over `accounts`, running commands in `workspace`.
    pub fn new(accounts: Arc<dyn IdentityStore>, workspace: impl Into<PathBuf>) -> Self {
        Self {
            accounts,
            workspace: workspace.into(),
        }
    }

    async fn exec(
        &self,
        call: ToolCall,
        progress: &mut dyn rhema_core::ToolProgress,
    ) -> ToolResult {
        let id = match call.args.get("id").and_then(Value::as_str) {
            Some(id) => id.trim().to_owned(),
            None => return ToolResult::failure(call.id, "missing required argument: id"),
        };
        let command = match call.args.get("command").and_then(Value::as_str) {
            Some(command) => command,
            None => return ToolResult::failure(call.id, "missing required argument: command"),
        };
        let timeout = match call.args.get("timeout_secs") {
            None | Some(Value::Null) => EXEC_TIMEOUT,
            Some(value) => {
                let Some(secs) = value.as_u64() else {
                    return ToolResult::failure(call.id, "timeout_secs must be a number");
                };
                Duration::from_secs(secs.clamp(1, EXEC_TIMEOUT_MAX.as_secs()))
            }
        };
        let Some(account) = self
            .accounts
            .get(&id)
            .map_err(|err| err.to_string())
            .ok()
            .flatten()
        else {
            return ToolResult::failure(
                call.id,
                format!("no account {id:?} — identity_list names them"),
            );
        };
        let Some(secret) = self
            .accounts
            .secret(&id)
            .map_err(|err| err.to_string())
            .ok()
            .flatten()
        else {
            return ToolResult::failure(call.id, format!("account {id:?} has no secret"));
        };

        let mut cmd = tokio::process::Command::new("bash");
        cmd.arg("-c")
            .arg(command)
            .current_dir(&self.workspace)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .env("ACCOUNT_ID", &account.id)
            .env("ACCOUNT_SERVICE", &account.service)
            .env(
                "ACCOUNT_USERNAME",
                account.username.as_deref().unwrap_or(""),
            )
            .env("ACCOUNT_SECRET", &secret.value)
            // git asks https credentials from the helper, which reads
            // the injected env; a failed auth must fail, not hang.
            .env("GIT_CONFIG_COUNT", "1")
            .env("GIT_CONFIG_KEY_0", "credential.helper")
            .env("GIT_CONFIG_VALUE_0", GIT_CREDENTIAL_HELPER)
            .env("GIT_TERMINAL_PROMPT", "0");
        if secret.kind == SecretKind::Token {
            for name in service_env_names(&account.service) {
                cmd.env(name, &secret.value);
            }
        }

        let mut child = match cmd.spawn() {
            Ok(child) => child,
            Err(err) => {
                return ToolResult::failure(call.id, format!("failed to spawn bash: {err}"));
            }
        };
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        if let Some(stdout) = child.stdout.take() {
            pump(stdout, false, tx.clone());
        }
        if let Some(stderr) = child.stderr.take() {
            pump(stderr, true, tx.clone());
        }
        drop(tx);
        let deadline = tokio::time::Instant::now() + timeout;
        let mut stdout: Vec<u8> = Vec::new();
        let mut stderr: Vec<u8> = Vec::new();
        loop {
            tokio::select! {
                _ = tokio::time::sleep_until(deadline) => {
                    let _ = child.start_kill();
                    let _ = child.wait().await;
                    return ToolResult::failure(
                        call.id,
                        format!(
                            "command timed out after {}s and was killed",
                            timeout.as_secs()
                        ),
                    );
                }
                line = rx.recv() => match line {
                    Some((is_stderr, line)) => {
                        let sink = if is_stderr { &mut stderr } else { &mut stdout };
                        sink.extend_from_slice(line.as_bytes());
                        sink.push(b'\n');
                        progress.output(&format!("{line}\n"));
                    }
                    None => break,
                }
            }
        }
        let status = match tokio::time::timeout(timeout, child.wait()).await {
            Ok(Ok(status)) => status,
            _ => {
                return ToolResult::failure(
                    call.id,
                    format!(
                        "command timed out after {}s and was killed",
                        timeout.as_secs()
                    ),
                );
            }
        };
        let content = format!(
            "exit code {}\n--- stdout ---\n{}\n--- stderr ---\n{}",
            status
                .code()
                .map(|code| code.to_string())
                .unwrap_or_else(|| "signal".to_owned()),
            render_truncated(&stdout),
            render_truncated(&stderr),
        );
        let result = if status.success() {
            ToolResult::success(call.id, content)
        } else {
            ToolResult::failure(call.id, content)
        };
        match status.code() {
            Some(code) => result.with_code(code),
            None => result,
        }
    }
}

/// Reads `stream` line by line into `tx`, tagged with its stream, until
/// EOF — the live side of the command's output.
fn pump<R>(stream: R, is_stderr: bool, tx: tokio::sync::mpsc::UnboundedSender<(bool, String)>)
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        use tokio::io::AsyncBufReadExt;
        let mut lines = tokio::io::BufReader::new(stream).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if tx.send((is_stderr, line)).is_err() {
                break;
            }
        }
    });
}

#[async_trait]
impl ToolResolver for IdentityExec {
    fn specs(&self) -> Vec<ToolSpec> {
        vec![ToolSpec::new(
            IDENTITY_EXEC,
            "Act with a stored account's credential without ever seeing \
             it: run one shell command in this session's workspace with \
             the secret injected as environment variables — ACCOUNT_SECRET, \
             the service's own (GITHUB_TOKEN/GH_TOKEN, GITEA_TOKEN, \
             GITLAB_TOKEN) — and a git credential helper preconfigured, \
             so plain `git clone`/`git push` over https and API calls \
             (curl, gh) just work. Only the command's output comes back; \
             the secret itself never enters the conversation. Pick `id` \
             from identity_list.",
            json!({
                "type": "object",
                "properties": {
                    "id": { "type": "string",
                        "description": "The account's id, as identity_list \
                            reports it." },
                    "command": { "type": "string",
                        "description": "The shell command to run." },
                    "timeout_secs": { "type": "integer",
                        "description": "Kill the command after this many \
                            seconds; 180 by default, 600 at most." }
                },
                "required": ["id", "command"]
            }),
        )]
    }

    async fn resolve(&self, call: ToolCall, _ctx: &ToolContext) -> ToolResult {
        self.exec(call, &mut rhema_core::OutputOnly(|_| {})).await
    }

    async fn resolve_streaming(
        &self,
        call: ToolCall,
        ctx: &ToolContext,
        progress: &mut dyn rhema_core::ToolProgress,
    ) -> ToolResult {
        if ctx.cancelled.load(std::sync::atomic::Ordering::Relaxed) {
            return ToolResult::failure(call.id, "cancelled");
        }
        self.exec(call, progress).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rhema_autonomous_agent::ToolOp;

    fn parts() -> (Arc<dyn IdentityStore>, Arc<ClawStore>) {
        (
            Arc::new(rhema_identity::VolatileIdentityStore::new()),
            Arc::new(ClawStore::in_memory().expect("store opens")),
        )
    }

    fn call(tool: &dyn ToolOp, args: Value) -> Result<String, String> {
        futures_block_on(tool.invoke(args))
    }

    #[test]
    fn every_manifest_loads_against_its_builtin_and_serves_the_registry() {
        let (accounts, watches) = parts();
        let (tools, skipped) = rhema_autonomous_agent::manifest::builtin_tools(
            MANIFESTS,
            &builtins(accounts, watches),
        );
        assert!(skipped.is_empty(), "{skipped:?}");
        assert_eq!(tools.len(), 3);
        let (accounts, watches) = parts();
        let resolver = registry(accounts, watches)
            .resolver_for(&rhema_autonomous_agent::Allowlist::permissive());
        assert_eq!(
            resolver.replay_safety("identity_list"),
            rhema_core::ReplaySafety::Safe
        );
        assert_eq!(
            resolver.replay_safety("identity_save"),
            rhema_core::ReplaySafety::Unsafe,
            "re-running a save on resume would re-store and re-watch"
        );
        assert_eq!(
            resolver.replay_safety("identity_forget"),
            rhema_core::ReplaySafety::Unsafe,
            "re-running a forget on resume would drop the account twice-over"
        );
    }

    /// The tools are async over a sync store: one block-on harness for
    /// the tests.
    fn futures_block_on<F: std::future::Future>(future: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime builds")
            .block_on(future)
    }

    #[test]
    fn save_answers_without_the_secret_and_watch_lands_for_a_known_forge() {
        let (accounts, watches) = parts();
        let tool = IdentitySave {
            accounts: accounts.clone(),
            watches: watches.clone(),
        };
        let answer = call(
            &tool,
            json!({
                "label": "acme bot",
                "service": "github",
                "secret": "sekrit-token",
                "watch": true
            }),
        )
        .expect("saves");
        assert!(!answer.contains("sekrit-token"), "{answer}");
        assert!(answer.contains("\"watched\":true"), "{answer}");

        let listed = call(
            &IdentityList {
                accounts: accounts.clone(),
            },
            json!({}),
        )
        .expect("lists");
        assert!(!listed.contains("sekrit-token"), "{listed}");
        assert!(listed.contains("acme bot"), "{listed}");

        let watched = watches.list_account_watches().expect("watches read");
        assert_eq!(watched.len(), 1);
        assert_eq!(watched[0].forge, "github");
    }

    #[test]
    fn forget_cascades_the_derived_watch() {
        let (accounts, watches) = parts();
        let saved = save_account(
            accounts.as_ref(),
            watches.as_ref(),
            AccountDraft::new("acme", "github", Secret::new(SecretKind::Token, "t")),
            true,
        )
        .expect("saves");
        assert!(saved.watch_added);

        let tool = IdentityForget {
            accounts: accounts.clone(),
            watches: watches.clone(),
        };
        let answer = call(&tool, json!({ "id": saved.account.id })).expect("forgets");
        assert!(answer.contains("\"forgot\":true"), "{answer}");
        assert!(answer.contains("\"watches_removed\":1"), "{answer}");
        assert!(watches.list_account_watches().expect("reads").is_empty());
        assert!(!accounts.secret(&saved.account.id).expect("reads").is_some());
    }

    #[test]
    fn a_watch_request_on_an_unknown_service_saves_without_watching() {
        let (accounts, watches) = parts();
        let saved = save_account(
            accounts.as_ref(),
            watches.as_ref(),
            AccountDraft::new("mail", "smtp", Secret::new(SecretKind::Password, "p"))
                .with_url("https://mail.example.net"),
            true,
        )
        .expect("saves");
        assert!(!saved.watch_added);
        assert!(!saved.forge_known);
        assert!(watches.list_account_watches().expect("reads").is_empty());
    }

    #[test]
    fn forge_of_matches_services_and_production_hosts_only() {
        assert_eq!(forge_of("GitHub", None), Some(ForgeKind::GitHub));
        assert_eq!(
            forge_of("whatever", Some("https://github.com/acme")),
            Some(ForgeKind::GitHub)
        );
        assert_eq!(
            forge_of("whatever", Some("https://gitlab.com/acme/w")),
            Some(ForgeKind::GitLab)
        );
        assert_eq!(
            forge_of("whatever", Some("https://git.example.net/acme/w")),
            None,
            "a self-hosted host is unrecognizable by URL alone"
        );
        assert_eq!(
            forge_of("gitea", Some("https://git.example.net")),
            Some(ForgeKind::Gitea)
        );
    }

    #[test]
    fn an_unknown_secret_kind_is_rejected_in_band() {
        let (accounts, watches) = parts();
        let tool = IdentitySave { accounts, watches };
        let err = call(
            &tool,
            json!({
                "label": "x",
                "service": "github",
                "secret": "t",
                "secret_kind": "pin"
            }),
        )
        .expect_err("rejected");
        assert!(err.contains("unknown secret_kind"), "{err}");
    }

    /// The use half of the accounts: the command runs with the
    /// service's env injected, the git helper configured, and none of
    /// the secret in the result.
    #[tokio::test]
    async fn identity_exec_runs_with_the_credential_in_its_environment() {
        let accounts = Arc::new(rhema_identity::VolatileIdentityStore::new());
        let stored = accounts
            .put(
                AccountDraft::new(
                    "github releases",
                    "github",
                    Secret::new(SecretKind::Token, "tok-1234567890"),
                )
                .with_username("kosmos-ai-bot"),
            )
            .expect("the account stores");
        let workspace = tempfile::tempdir().expect("tempdir");
        let exec = IdentityExec::new(accounts, workspace.path());
        let id = stored.id.clone();

        async fn run(exec: &IdentityExec, id: &str, command: &str) -> rhema_core::ToolResult {
            exec.resolve(
                ToolCall {
                    id: "c1".into(),
                    name: IDENTITY_EXEC.into(),
                    args: json!({ "id": id, "command": command }),
                    provider_data: Value::Null,
                },
                &rhema_core::ToolContext::default(),
            )
            .await
        }

        // The service's token and the generic set are all the same
        // value, and the username rode along.
        let result = run(
            &exec,
            &id,
            "[ -n \"$GITHUB_TOKEN\" ] && [ \"$GH_TOKEN\" = \"$ACCOUNT_SECRET\" ] \
             && [ \"$ACCOUNT_USERNAME\" = \"kosmos-ai-bot\" ] \
             && [ \"$GIT_CONFIG_COUNT\" = \"1\" ] && echo ok",
        )
        .await;
        assert!(!result.is_error, "{}", result.content);
        assert!(result.content.contains("ok"), "{}", result.content);
        assert!(
            !result.content.contains("tok-1234567890"),
            "the secret never rides the answer: {}",
            result.content
        );

        // The helper answers git's prompt shape from the env.
        let result = run(
            &exec,
            &id,
            "printf 'username=%s password=%s' \"$ACCOUNT_USERNAME\" \"$ACCOUNT_SECRET\" \
             | grep -c kosmos-ai-bot",
        )
        .await;
        assert!(!result.is_error, "{}", result.content);
    }

    #[tokio::test]
    async fn identity_exec_reports_exit_codes_and_unknown_accounts() {
        let accounts = Arc::new(rhema_identity::VolatileIdentityStore::new());
        let stored = accounts
            .put(AccountDraft::new(
                "gitea",
                "gitea",
                Secret::new(SecretKind::Token, "gt-1"),
            ))
            .expect("the account stores");
        let workspace = tempfile::tempdir().expect("tempdir");
        let exec = IdentityExec::new(accounts.clone(), workspace.path());
        let id = stored.id.clone();

        let call = |id: &str, args: Value| ToolCall {
            id: "c1".into(),
            name: IDENTITY_EXEC.into(),
            args: {
                let mut args = args;
                args["id"] = json!(id);
                args
            },
            provider_data: Value::Null,
        };
        let failure = exec
            .resolve(
                call(&id, json!({ "command": "exit 3" })),
                &rhema_core::ToolContext::default(),
            )
            .await;
        assert!(failure.is_error);
        assert_eq!(failure.code, Some(3));
        assert!(failure.content.contains("exit code 3"));

        // A gitea token answers as GITEA_TOKEN; a github account it is not.
        let check = exec
            .resolve(
                call(
                    &id,
                    json!({
                        "command": "[ -n \"$GITEA_TOKEN\" ] && [ -z \"$GITHUB_TOKEN\" ]"
                    }),
                ),
                &rhema_core::ToolContext::default(),
            )
            .await;
        assert!(!check.is_error, "{}", check.content);

        let missing = exec
            .resolve(
                call("99", json!({ "command": "echo hi" })),
                &rhema_core::ToolContext::default(),
            )
            .await;
        assert!(missing.is_error);
        assert!(
            missing.content.contains("no account"),
            "{}",
            missing.content
        );

        // The command runs in the session's workspace.
        let pwd = exec
            .resolve(
                call(&id, json!({ "command": "pwd" })),
                &rhema_core::ToolContext::default(),
            )
            .await;
        assert!(
            pwd.content
                .contains(workspace.path().to_str().expect("utf8")),
            "{}",
            pwd.content
        );
    }
}
