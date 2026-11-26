//! The MCP config a session actually uses: one JSON file inside the
//! sandbox (or the host, for the local session), never the host profile.
//!
//! The `mcp` client reads the same file through `MCP_CONFIG`. The
//! prompt is its `mcp prompt` output, so the model sees the servers and
//! their descriptions before it decides a command is irrelevant.

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use crate::mcp::McpServerEntry;
use base64::Engine as _;
use rhema_bash::runtime::KIT_VERSION;
use rhema_sandbox::kit::SharedRoot;
use rhema_sandbox::{OutputOnly, RunRequest, Sandbox};

/// The file name under the MCP directory.
const CONFIG_FILE: &str = "config.json";

/// A parsed `{"servers": {...}}` file. The flat map and Claude's
/// `mcpServers` spelling parse too, so a hand-written file works.
#[derive(serde::Deserialize, serde::Serialize)]
struct File {
    servers: BTreeMap<String, McpServerEntry>,
}

/// Where the config lives and how the host reaches it.
#[derive(Clone)]
enum Place {
    /// The local session: a host file.
    Host { config: PathBuf, media: PathBuf },
    /// A backend that mounts the runtime root: both sides read the
    /// same file, so the host writes it directly.
    Shared {
        config_host: PathBuf,
        media_host: PathBuf,
        config: String,
        media: String,
        sandbox: Arc<dyn Sandbox>,
    },
    /// A backend with no shared file system: the host reads and writes
    /// through the sandbox's shell.
    Remote {
        config: String,
        media: String,
        sandbox: Arc<dyn Sandbox>,
    },
}

/// The session's MCP state: entries the settings page edits, the
/// environment the shell and the probe use, and the prompt section.
#[derive(Clone)]
pub(crate) struct McpStore {
    pub(crate) entries: BTreeMap<String, McpServerEntry>,
    /// The `mcp prompt` output, absent when the client is missing or
    /// the file holds no servers.
    pub(crate) prompt: Option<String>,
    /// `MCP_CONFIG`, `MCP_MEDIA_DIR`, `MCP_DISABLED`, and `PATH`.
    pub(crate) env: BTreeMap<String, String>,
    /// The names the session muted. The client filters them from
    /// `list` and `prompt`; a direct call still works.
    disabled: BTreeSet<String>,
    place: Place,
    workspace: String,
    /// The directory the client was installed into, prepended to
    /// `PATH` so `mcp` resolves without a global install.
    bin_dir: Option<String>,
    /// The PATH the sandbox already had. `None` leaves `PATH` alone,
    /// which keeps a shell's default in place.
    base_path: Option<String>,
}

impl McpStore {
    /// The local session's store: `~/.config/rhema/mcp/config.json`,
    /// media under the cache.
    pub(crate) fn local(home: Option<&rhema_config::Home>, disabled: &BTreeSet<String>) -> Self {
        let home = home
            .cloned()
            .or_else(|| rhema_config::Home::discover().ok());
        let (config, media) = match &home {
            Some(home) => (
                home.config.join("mcp").join(CONFIG_FILE),
                home.cache.join("mcp").join("media"),
            ),
            None => (
                PathBuf::from(".rhema/mcp").join(CONFIG_FILE),
                PathBuf::from(".rhema/mcp/media"),
            ),
        };
        let _ = std::fs::create_dir_all(&media);
        let entries = read_host(&config);
        let bin_dir = local_bin_dir(home.as_ref());
        let base_path = std::env::var("PATH").ok();
        let mut store = Self {
            entries,
            prompt: None,
            env: BTreeMap::new(),
            disabled: disabled.clone(),
            place: Place::Host { config, media },
            workspace: String::new(),
            bin_dir,
            base_path,
        };
        store.refresh_env();
        store.prompt = store.probe_host();
        store
    }

    /// A sandbox session's store. A mounted backend puts the file in
    /// the shared runtime root, where the host and the sandbox both
    /// reach it. A backend without that root keeps it in the sandbox's
    /// home.
    pub(crate) async fn for_sandbox(
        sandbox: Arc<dyn Sandbox>,
        shared: Option<&SharedRoot>,
        target: &str,
        workspace: &str,
        disabled: &BTreeSet<String>,
    ) -> Self {
        let (place, _config_guest, _media_guest, bin_dir, base_path) = match shared {
            Some(shared) => {
                let scope = scope_name(target);
                let host = shared.host.join("mcp").join(&scope);
                let _ = std::fs::create_dir_all(&host);
                let config_host = host.join(CONFIG_FILE);
                let media_host = host.join("media");
                let guest = format!("{}/mcp/{scope}", shared.sandbox.trim_end_matches('/'));
                let config = format!("{guest}/{CONFIG_FILE}");
                let media = format!("{guest}/media");
                let bin_dir = Some(format!(
                    "{}/bin/{KIT_VERSION}",
                    shared.sandbox.trim_end_matches('/')
                ));
                let base_path = run_text(
                    Arc::clone(&sandbox),
                    "printf %s \"$PATH\"",
                    workspace,
                    BTreeMap::new(),
                )
                .await;
                (
                    Place::Shared {
                        config_host,
                        media_host,
                        config: config.clone(),
                        media: media.clone(),
                        sandbox: Arc::clone(&sandbox),
                    },
                    config,
                    media,
                    bin_dir,
                    base_path,
                )
            }
            None => {
                // Resolve the home once: the settings screen's reads
                // and writes quote `$MCP_CONFIG`, and a shell does not
                // re-expand a variable's value. A failed probe keeps
                // the variable form, which the client expands itself.
                let home = run_text(
                    Arc::clone(&sandbox),
                    "printf %s \"$HOME\"",
                    workspace,
                    BTreeMap::new(),
                )
                .await
                .unwrap_or_default();
                let home = if home.starts_with('/') {
                    home.trim_end_matches('/').to_owned()
                } else {
                    "$HOME".to_owned()
                };
                let config = format!("{home}/.config/rhema/mcp/config.json");
                let media = format!("{home}/.cache/rhema/mcp/media");
                // The sandbox-side installer puts `mcp` in this cache;
                // `~/.local/bin` stays a fallback for a hand-installed
                // client.
                let bin_dir = Some(format!(
                    "{home}/.cache/rhema/kit/{KIT_VERSION}:{home}/.local/bin"
                ));
                let base_path = run_text(
                    Arc::clone(&sandbox),
                    "printf %s \"$PATH\"",
                    workspace,
                    BTreeMap::new(),
                )
                .await;
                (
                    Place::Remote {
                        config: config.clone(),
                        media: media.clone(),
                        sandbox: Arc::clone(&sandbox),
                    },
                    config,
                    media,
                    bin_dir,
                    base_path,
                )
            }
        };
        let mut store = Self {
            entries: BTreeMap::new(),
            prompt: None,
            env: BTreeMap::new(),
            disabled: disabled.clone(),
            place,
            workspace: workspace.to_owned(),
            bin_dir,
            base_path,
        };
        store.refresh_env();
        store.entries = store.read().await.unwrap_or_default();
        // The media directory must exist before the first media result.
        let _ = store.ensure_media().await;
        store.prompt = store.probe().await;
        store
    }

    /// The settings page's rows, in name order.
    pub(crate) fn entries(&self) -> &BTreeMap<String, McpServerEntry> {
        &self.entries
    }

    /// The dialog's accepted entry.
    pub(crate) fn insert(&mut self, name: String, entry: McpServerEntry) {
        self.entries.insert(name, entry);
    }

    /// The removal action.
    pub(crate) fn remove(&mut self, name: &str) -> bool {
        self.entries.remove(name).is_some()
    }

    /// The session's mute set changed: rewrite the client env and
    /// refresh the prompt section.
    pub(crate) fn set_disabled(&mut self, disabled: BTreeSet<String>) {
        self.disabled = disabled;
        self.refresh_env();
    }

    /// The async twin for a caller already on a runtime: writes the
    /// mute into the client environment and probes again without
    /// parking the reactor.
    pub(crate) async fn set_disabled_async(&mut self, disabled: BTreeSet<String>) {
        self.disabled = disabled;
        self.refresh_env();
        self.prompt = self.probe().await;
    }

    /// Persists the entries and refreshes the prompt. Returns a message
    /// on failure, never aborts the session.
    pub(crate) fn save_and_probe(&mut self) -> Result<(), String> {
        let raw = serde_json::to_string_pretty(&File {
            servers: self.entries.clone(),
        })
        .map_err(|err| err.to_string())?;
        match &self.place {
            Place::Host { config, .. } => {
                if let Some(parent) = config.parent() {
                    std::fs::create_dir_all(parent).map_err(|err| err.to_string())?;
                }
                std::fs::write(config, raw).map_err(|err| err.to_string())?;
                self.prompt = self.probe_host();
            }
            Place::Shared { config_host, .. } => {
                if let Some(parent) = config_host.parent() {
                    std::fs::create_dir_all(parent).map_err(|err| err.to_string())?;
                }
                std::fs::write(config_host, raw).map_err(|err| err.to_string())?;
                self.prompt = block(self.probe_shared());
            }
            Place::Remote { .. } => {
                block(self.write_remote(&raw))?;
                self.prompt = block(self.probe_remote());
            }
        }
        Ok(())
    }

    /// Recomputes the prompt. Called after a mute flip, which changes
    /// no file but changes what the prompt advertises.
    pub(crate) fn reprobe(&mut self) {
        self.prompt = match &self.place {
            Place::Host { .. } => self.probe_host(),
            Place::Shared { .. } => block(self.probe_shared()),
            Place::Remote { .. } => block(self.probe_remote()),
        };
    }

    /// The client env, refreshed from the current place and mute set.
    fn refresh_env(&mut self) {
        let (config, media) = match &self.place {
            Place::Host { config, media } => (
                config.to_string_lossy().into_owned(),
                media.to_string_lossy().into_owned(),
            ),
            Place::Shared { config, media, .. } => (config.clone(), media.clone()),
            Place::Remote { config, media, .. } => (config.clone(), media.clone()),
        };
        self.env.insert("MCP_CONFIG".to_owned(), config);
        self.env.insert("MCP_MEDIA_DIR".to_owned(), media);
        self.env.insert(
            "MCP_DISABLED".to_owned(),
            self.disabled.iter().cloned().collect::<Vec<_>>().join(","),
        );
        // The init file only runs when a session places its kit. Put
        // the install directory on PATH here, so the model's `mcp` and
        // the prompt probe resolve the same binary on every backend.
        if let (Some(bin), Some(base)) = (&self.bin_dir, &self.base_path) {
            let path = if base.is_empty() {
                format!("{bin}:/usr/local/bin:/usr/bin:/bin")
            } else {
                format!("{bin}:{base}")
            };
            self.env.insert("PATH".to_owned(), path);
        }
    }

    async fn read(&self) -> Option<BTreeMap<String, McpServerEntry>> {
        match &self.place {
            Place::Host { config, .. } => Some(read_host(config)),
            Place::Shared { config_host, .. } => Some(read_host(config_host)),
            Place::Remote { .. } => self.read_remote().await,
        }
    }

    async fn ensure_media(&self) -> Result<(), String> {
        match &self.place {
            Place::Host { media, .. } => {
                std::fs::create_dir_all(media).map_err(|err| err.to_string())?;
                Ok(())
            }
            Place::Shared { media_host, .. } => {
                std::fs::create_dir_all(media_host).map_err(|err| err.to_string())?;
                Ok(())
            }
            Place::Remote { .. } => {
                let outcome = self
                    .run("mkdir -p \"$MCP_MEDIA_DIR\"", BTreeMap::new())
                    .await;
                match outcome {
                    Ok((0, _, _)) => Ok(()),
                    Ok((code, _, err)) => Err(format!(
                        "mkdir exited {code}: {}",
                        String::from_utf8_lossy(&err)
                    )),
                    Err(err) => Err(err),
                }
            }
        }
    }

    async fn read_remote(&self) -> Option<BTreeMap<String, McpServerEntry>> {
        let outcome = self
            .run("cat \"$MCP_CONFIG\" 2>/dev/null", BTreeMap::new())
            .await
            .ok()?;
        if outcome.0 != 0 {
            return None;
        }
        Some(parse(&String::from_utf8_lossy(&outcome.1)))
    }

    async fn write_remote(&self, raw: &str) -> Result<(), String> {
        let encoded = base64::engine::general_purpose::STANDARD.encode(raw);
        let command = format!(
            "mkdir -p \"$(dirname \"$MCP_CONFIG\")\" && printf %s '{}' | base64 -d > \"$MCP_CONFIG\"",
            encoded
        );
        match self.run(&command, BTreeMap::new()).await {
            Ok((0, _, _)) => Ok(()),
            Ok((code, _, err)) => Err(format!(
                "the MCP config write exited {code}: {}",
                String::from_utf8_lossy(&err)
            )),
            Err(err) => Err(err),
        }
    }

    fn probe_host(&self) -> Option<String> {
        // `Command` resolves a bare name with the parent's PATH, not
        // the child's environment, so try the install directory
        // explicitly before falling back to an inherited `mcp`.
        let program = std::env::var("MCP_BIN").ok().unwrap_or_else(|| {
            self.bin_dir
                .as_ref()
                .map(|dir| format!("{dir}/mcp"))
                .filter(|path| Path::new(path).is_file())
                .unwrap_or_else(|| "mcp".to_owned())
        });
        let output = std::process::Command::new(program)
            .arg("prompt")
            .envs(&self.env)
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let text = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        (!text.is_empty()).then_some(text)
    }

    async fn probe(&self) -> Option<String> {
        match &self.place {
            Place::Host { .. } => self.probe_host(),
            Place::Shared { .. } => self.probe_shared().await,
            Place::Remote { .. } => self.probe_remote().await,
        }
    }

    async fn probe_shared(&self) -> Option<String> {
        let bin = match &self.place {
            Place::Shared { .. } => format!("{}/mcp", self.bin_dir.as_deref()?),
            _ => return None,
        };
        let outcome = self
            .run(&format!("\"{bin}\" prompt"), BTreeMap::new())
            .await
            .ok()?;
        if outcome.0 != 0 {
            return None;
        }
        let text = String::from_utf8_lossy(&outcome.1).trim().to_owned();
        (!text.is_empty()).then_some(text)
    }

    async fn probe_remote(&self) -> Option<String> {
        let outcome = self.run("mcp prompt", BTreeMap::new()).await.ok()?;
        if outcome.0 != 0 {
            return None;
        }
        let text = String::from_utf8_lossy(&outcome.1).trim().to_owned();
        (!text.is_empty()).then_some(text)
    }

    async fn run(
        &self,
        command: &str,
        extra: BTreeMap<String, String>,
    ) -> Result<(i32, Vec<u8>, Vec<u8>), String> {
        let sandbox = match &self.place {
            Place::Shared { sandbox, .. } | Place::Remote { sandbox, .. } => sandbox.clone(),
            Place::Host { .. } => return Err("the local store runs on the host".to_owned()),
        };
        let mut env = self.env.clone();
        env.extend(extra);
        let cancel = std::sync::atomic::AtomicBool::new(false);
        let mut progress = OutputOnly(|_| {});
        let outcome = sandbox
            .run(
                RunRequest {
                    command: command.to_owned(),
                    cwd: PathBuf::from(&self.workspace),
                    timeout: Duration::from_secs(10),
                    env,
                },
                &cancel,
                &mut progress,
            )
            .await
            .map_err(|err| err.to_string())?;
        Ok((outcome.code.unwrap_or(-1), outcome.stdout, outcome.stderr))
    }
}

/// Blocks a sandbox call from the synchronous settings screen. The app
/// runs on the multi-thread runtime; `block_in_place` keeps a worker
/// free while this one waits.
fn block<T>(future: impl Future<Output = T> + Send) -> T
where
    T: Send,
{
    match tokio::runtime::Handle::try_current() {
        Ok(handle) if handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => {
            tokio::task::block_in_place(|| handle.block_on(future))
        }
        Ok(handle) => handle.block_on(future),
        Err(_) => {
            let runtime = tokio::runtime::Runtime::new().expect("the MCP store runtime builds");
            runtime.block_on(future)
        }
    }
}

/// Where a host-side `mcp` lives: `MCP_BIN`'s directory when it names
/// a path, else the installer's `~/.local/bin`.
fn local_bin_dir(home: Option<&rhema_config::Home>) -> Option<String> {
    if let Ok(bin) = std::env::var("MCP_BIN")
        && Path::new(&bin).is_absolute()
        && let Some(parent) = Path::new(&bin).parent()
    {
        return Some(parent.to_string_lossy().into_owned());
    }
    home.map(|home| home.home.join(".local/bin").to_string_lossy().into_owned())
        .or_else(|| {
            std::env::var_os("HOME").map(|home| {
                PathBuf::from(home)
                    .join(".local/bin")
                    .to_string_lossy()
                    .into_owned()
            })
        })
}

fn read_host(path: &Path) -> BTreeMap<String, McpServerEntry> {
    std::fs::read_to_string(path)
        .map(|raw| parse(&raw))
        .unwrap_or_default()
}

fn parse(raw: &str) -> BTreeMap<String, McpServerEntry> {
    let value: serde_json::Value = match serde_json::from_str(raw) {
        Ok(value) => value,
        Err(err) => {
            eprintln!("mcp: the config is not JSON: {err}");
            return BTreeMap::new();
        }
    };
    let map = ["servers", "mcpServers"]
        .iter()
        .find_map(|key| value.get(*key).cloned())
        .or_else(|| value.as_object().map(|_| value.clone()))
        .and_then(|value| value.as_object().cloned())
        .unwrap_or_default();
    let mut entries =
        serde_json::from_value::<BTreeMap<String, McpServerEntry>>(serde_json::Value::Object(map))
            .unwrap_or_else(|err| {
                eprintln!("mcp: the config entries are invalid: {err}");
                BTreeMap::new()
            });
    // Normalize the `command: "program", args: [...]` spelling into the
    // argv array `mcp-cli` spawns, so a later save writes one shape.
    for entry in entries.values_mut() {
        if let Some(command) = &mut entry.command {
            command.append(&mut entry.args);
        }
    }
    entries
}

/// Runs one command with no session env and returns its trimmed
/// stdout, for the home probe that has to happen before the store
/// exists.
async fn run_text(
    sandbox: Arc<dyn Sandbox>,
    command: &str,
    workspace: &str,
    env: BTreeMap<String, String>,
) -> Option<String> {
    let cancel = std::sync::atomic::AtomicBool::new(false);
    let mut progress = OutputOnly(|_| {});
    let outcome = sandbox
        .run(
            RunRequest {
                command: command.to_owned(),
                cwd: PathBuf::from(workspace),
                timeout: Duration::from_secs(5),
                env,
            },
            &cancel,
            &mut progress,
        )
        .await
        .ok()?;
    (outcome.code == Some(0)).then(|| String::from_utf8_lossy(&outcome.stdout).trim().to_owned())
}

/// A sandbox target as one directory name: `docker ps` names and
/// `http://host:port` both land under `mcp/` without escaping it.
fn scope_name(target: &str) -> String {
    let mut scope: String = target
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '-'
            }
        })
        .take(64)
        .collect();
    if scope.is_empty() {
        scope.push_str("default");
    }
    scope
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::sync::atomic::AtomicBool;

    use rhema_sandbox::{RunOutcome, SandboxError, ToolProgress, WorkspaceSeed};

    /// A sandbox that answers every command with the same prompt line
    /// and records what it was asked.
    #[derive(Default)]
    struct FakeSandbox {
        commands: Mutex<Vec<String>>,
    }

    #[async_trait::async_trait]
    impl Sandbox for FakeSandbox {
        async fn run(
            &self,
            request: RunRequest,
            _cancelled: &AtomicBool,
            _progress: &mut dyn ToolProgress,
        ) -> Result<RunOutcome, SandboxError> {
            self.commands.lock().unwrap().push(request.command);
            Ok(RunOutcome {
                stdout: b"MCP servers installed:\n- jira: Jira".to_vec(),
                stderr: Vec::new(),
                code: Some(0),
            })
        }

        async fn seed(&self, _seed: &WorkspaceSeed) -> Result<(), SandboxError> {
            Ok(())
        }

        async fn read_file(&self, _path: &str) -> Result<Vec<u8>, SandboxError> {
            Ok(Vec::new())
        }

        async fn write_file(&self, _path: &str, _contents: &[u8]) -> Result<(), SandboxError> {
            Ok(())
        }
    }

    fn entry() -> McpServerEntry {
        McpServerEntry {
            description: "Jira".to_owned(),
            command: Some(vec!["uvx".to_owned(), "jira-mcp".to_owned()]),
            url: None,
            env: BTreeMap::new(),
            headers: BTreeMap::new(),
            args: Vec::new(),
        }
    }

    #[test]
    fn the_local_store_writes_the_json_the_client_reads() {
        let dir = tempfile::tempdir().unwrap();
        let home = rhema_config::Home::under_home(dir.path());
        let mut store = McpStore::local(Some(&home), &BTreeSet::new());
        store.insert("jira".to_owned(), entry());
        store.save_and_probe().unwrap();
        let path = home.config.join("mcp/config.json");
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.contains("\"servers\""));
        assert!(raw.contains("\"jira\""));
        let reloaded = McpStore::local(Some(&home), &BTreeSet::new());
        assert_eq!(reloaded.entries().len(), 1);
        assert_eq!(
            reloaded.entries()["jira"].command.as_deref(),
            Some(["uvx".to_owned(), "jira-mcp".to_owned()].as_slice())
        );
    }

    #[test]
    fn the_client_string_command_and_args_load_as_one_argv() {
        let dir = tempfile::tempdir().unwrap();
        let home = rhema_config::Home::under_home(dir.path());
        let path = home.config.join("mcp/config.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            r#"{"servers":{"jira":{"command":"uvx","args":["jira-mcp"],"env":{"TOKEN":"x"}}}}"#,
        )
        .unwrap();
        let mut store = McpStore::local(Some(&home), &BTreeSet::new());
        assert_eq!(
            store.entries()["jira"].command.as_deref(),
            Some(["uvx".to_owned(), "jira-mcp".to_owned()].as_slice())
        );
        assert!(store.entries()["jira"].args.is_empty());
        store.save_and_probe().unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(!raw.contains("\"args\""), "{raw}");
    }

    #[test]
    fn the_client_directory_leads_path() {
        let dir = tempfile::tempdir().unwrap();
        let home = rhema_config::Home::under_home(dir.path());
        let store = McpStore::local(Some(&home), &BTreeSet::new());
        let path = store.env.get("PATH").expect("PATH");
        assert!(
            path.starts_with(&format!("{}:", home.home.join(".local/bin").display())),
            "{path}"
        );
        assert!(path.contains(&std::env::var("PATH").unwrap_or_default()));
    }

    #[test]
    fn the_mute_reaches_the_client_environment() {
        let dir = tempfile::tempdir().unwrap();
        let home = rhema_config::Home::under_home(dir.path());
        let mut store = McpStore::local(Some(&home), &BTreeSet::new());
        store.set_disabled(BTreeSet::from(["jira".to_owned()]));
        assert_eq!(
            store.env.get("MCP_DISABLED").map(String::as_str),
            Some("jira")
        );
    }

    #[tokio::test]
    async fn the_shared_probe_uses_the_kit_bin_directory() {
        let sandbox = Arc::new(FakeSandbox::default());
        let store = McpStore {
            entries: BTreeMap::new(),
            prompt: None,
            env: BTreeMap::new(),
            disabled: BTreeSet::new(),
            place: Place::Shared {
                config_host: PathBuf::from("/tmp/rhema-mcp-test/config.json"),
                media_host: PathBuf::from("/tmp/rhema-mcp-test/media"),
                config: "/run/rhema/mcp/box/config.json".to_owned(),
                media: "/run/rhema/mcp/box/media".to_owned(),
                sandbox: sandbox.clone(),
            },
            workspace: "/workspace".to_owned(),
            bin_dir: Some("/run/rhema/bin/3".to_owned()),
            base_path: None,
        };
        assert_eq!(
            store.probe_shared().await.as_deref(),
            Some("MCP servers installed:\n- jira: Jira")
        );
        assert_eq!(
            sandbox.commands.lock().unwrap().as_slice(),
            ["\"/run/rhema/bin/3/mcp\" prompt"]
        );
    }

    #[test]
    fn a_target_becomes_one_directory_name() {
        assert_eq!(scope_name("rhema-sandbox-proj-1"), "rhema-sandbox-proj-1");
        assert_eq!(scope_name("http://box:7331"), "http---box-7331");
        assert_eq!(scope_name(""), "default");
    }
}
