//! The wired whole, through the real binary. `wire_turn.rs` in
//! rhema-mcp walks the discovery story against the standalone
//! `rhema-mcp-cli`; `mcp_routing.rs` here pins the `rhema` binary's
//! routing with no bridge behind it. This test is the middle both
//! leave open: the wire provisions an agent shell whose
//! `RHEMA_MCP_BIN` is the running frontend itself, the production
//! shape, and the scripted model walks `mcp servers`, the generated
//! command's `--help`, one tool's `--help`, and the call through it.
//!
//! The binary re-executes itself with `RHEMA_MCP_WIRE_FAKE` set to
//! play the fake stdio MCP server (the fake_server.rs harness
//! pattern: no node, no network); without the variable, `main` runs
//! the one test.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use rhema_config::mcp::{McpConfig, McpServer, McpTransport};
use rhema_core::{
    Content, Event, EventSink, Message, ModelOutput, ModelProvider, ModelRequest, ProviderError,
    Role, StopReason, ToolCall, ToolResult, Usage,
};
use rhema_host_agent::{HostAgent, HostAgentConfig};
use serde_json::{Value, json};

/// The env var switching this binary into fake-server mode.
const FAKE_ENV: &str = "RHEMA_MCP_WIRE_FAKE";

/// Where the fake records one line per spawn; the test samples its
/// length on every request boundary.
const MARKER_ENV: &str = "RHEMA_WIRE_MARKER";

/// The config name and the generated bash function, one and the same.
const SERVER_NAME: &str = "validate-test-mcp";

fn main() {
    if let Ok(script) = std::env::var(FAKE_ENV) {
        play(&script);
        return;
    }
    let rt = tokio::runtime::Runtime::new().expect("test runtime");
    eprint!("the_real_binary_walks_the_discovery_story ... ");
    match rt.block_on(the_real_binary_walks_the_discovery_story()) {
        Ok(()) => eprintln!("ok"),
        Err(err) => {
            eprintln!("FAILED: {err}");
            std::process::exit(1);
        }
    }
}

// ---------------------------------------------------------------------
// The fake server
// ---------------------------------------------------------------------

/// Plays a scenario. One exists; unknown names behave as it, like the
/// fake_server harness.
fn play(script: &str) {
    let _ = script;
    validate_test_scenario(&std::env::var(MARKER_ENV).unwrap_or_default());
}

/// Reads one newline-delimited JSON-RPC request from stdin.
fn read_request() -> Option<Value> {
    let mut line = String::new();
    match std::io::stdin().read_line(&mut line) {
        Ok(0) | Err(_) => None,
        Ok(_) => serde_json::from_str(&line).ok(),
    }
}

/// Writes one envelope line and flushes.
fn send(value: Value) {
    println!(
        "{}",
        serde_json::to_string(&value).expect("fake serializes")
    );
    let _ = std::io::stdout().flush();
}

/// A result envelope for an id.
fn result_envelope(id: &Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

/// An error envelope for an id.
fn error_envelope(id: &Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

/// The handshake answer this client accepts.
fn handshake_result(version: &str) -> Value {
    json!({
        "protocolVersion": version,
        "capabilities": {},
        "serverInfo": {"name": "fake", "version": "0"}
    })
}

/// Handshake (one marker line per spawn), a one-tool catalog, canned
/// calls that echo the `path` argument back, the echo proves the
/// arguments crossed the rhema binary, bridge, manager, and server
/// intact.
fn validate_test_scenario(marker: &str) {
    loop {
        let Some(msg) = read_request() else { return };
        let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
        let id = msg.get("id").cloned();
        match method {
            "initialize" => {
                let Some(id) = id else { continue };
                if !marker.is_empty()
                    && let Ok(mut file) = std::fs::OpenOptions::new()
                        .append(true)
                        .create(true)
                        .open(marker)
                {
                    let _ = writeln!(file, "spawned {}", std::process::id());
                }
                send(result_envelope(&id, handshake_result("2025-06-18")));
            }
            "notifications/initialized" => {}
            "tools/list" => {
                let Some(id) = id else { continue };
                send(result_envelope(
                    &id,
                    json!({"tools": [{
                        "name": "validate-test",
                        "description": "Validate a test file",
                        "inputSchema": {
                            "type": "object",
                            "properties": {
                                "path": {"type": "string", "description": "File to validate"}
                            },
                            "required": ["path"]
                        }
                    }]}),
                ));
            }
            "tools/call" => {
                let Some(id) = id else { continue };
                let path = msg
                    .pointer("/params/arguments/path")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                send(result_envelope(
                    &id,
                    json!({
                        "content": [{"type": "text", "text": format!("validation ok: {path}")}],
                        "isError": false,
                    }),
                ));
            }
            _ => {
                let Some(id) = id else { continue };
                send(error_envelope(&id, -32601, "method not found"));
            }
        }
    }
}

// ---------------------------------------------------------------------
// The scripted model
// ---------------------------------------------------------------------

/// One served request, kept for the post-turn assertions.
#[derive(Clone)]
struct Recorded {
    /// The rendered system prompt.
    system: Option<String>,
    /// The tools array's names, for the never-grows check.
    tool_names: Vec<String>,
    /// Marker lines at request time. A request arrives only after the
    /// previous round's command fully ran, so the samples pin when the
    /// one spawn happened.
    spawns: usize,
}

/// The model: replays the scripted replies in order and records each
/// request. Thread-safe, like `MockProvider`; unlike it, it keeps the
/// requests, because the prompt and the tools array are half the
/// subject under test.
struct Scripted {
    replies: Mutex<VecDeque<ModelOutput>>,
    requests: Mutex<Vec<Recorded>>,
    marker: PathBuf,
}

impl Scripted {
    /// A model scripted with `replies`, sampling `marker` per request.
    fn new(replies: Vec<ModelOutput>, marker: &Path) -> Self {
        Self {
            replies: Mutex::new(replies.into_iter().collect()),
            requests: Mutex::new(Vec::new()),
            marker: marker.to_path_buf(),
        }
    }

    /// The recorded requests, cloned out after the turn.
    fn requests(&self) -> Vec<Recorded> {
        self.requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

#[async_trait]
impl ModelProvider for Scripted {
    async fn complete(
        &self,
        req: ModelRequest,
        _sink: &mut dyn EventSink,
    ) -> Result<ModelOutput, ProviderError> {
        self.requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(Recorded {
                system: req.system.clone(),
                tool_names: req.tools.iter().map(|spec| spec.name.clone()).collect(),
                spawns: marker_lines(&self.marker),
            });
        self.replies
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pop_front()
            .ok_or(ProviderError::Exhausted)
    }
}

/// A turn step: the model calls `bash` with `command`.
fn bash_call(id: &str, command: &str) -> ModelOutput {
    ModelOutput {
        message: Message {
            annotations: Default::default(),
            role: Role::Assistant,
            content: vec![Content::ToolCall(ToolCall {
                id: id.to_owned(),
                name: "bash".into(),
                args: json!({"command": command}),
                ..ToolCall::default()
            })],
        },
        stop_reason: StopReason::ToolUse,
        usage: Usage::default(),
    }
}

/// The turn's end.
fn end_turn(text: &str) -> ModelOutput {
    ModelOutput {
        message: Message::text(Role::Assistant, text),
        stop_reason: StopReason::EndTurn,
        usage: Usage::default(),
    }
}

struct NullSink;

impl EventSink for NullSink {
    fn emit(&mut self, _event: Event<'_>) {}
}

/// The marker file's line count, absent file counting as zero.
fn marker_lines(marker: &Path) -> usize {
    std::fs::read_to_string(marker)
        .map(|body| body.lines().count())
        .unwrap_or(0)
}

// ---------------------------------------------------------------------
// The test
// ---------------------------------------------------------------------

/// The production shape, whole: the wire provisions an agent shell
/// around the running frontend binary, the scripted model walks the
/// discovery story through it, and every answer lands in history as a
/// tool result. A routing break lands here too, the generated
/// functions exec the rhema binary, whose catch-all hands `__server`
/// to the MCP CLI.
async fn the_real_binary_walks_the_discovery_story() -> Result<(), String> {
    let workspace = tempfile::tempdir().map_err(|e| e.to_string())?;
    let session = tempfile::tempdir().map_err(|e| e.to_string())?;
    let marker = workspace.path().join("spawns");

    // One stdio server: the test binary re-execs itself as the fake.
    let mut server_env = BTreeMap::new();
    server_env.insert(FAKE_ENV.to_owned(), "validate-test".to_owned());
    server_env.insert(MARKER_ENV.to_owned(), marker.to_string_lossy().into_owned());
    let exe = std::env::current_exe()
        .map_err(|e| e.to_string())?
        .to_string_lossy()
        .into_owned();
    let mcp_config = McpConfig {
        servers: vec![McpServer {
            name: SERVER_NAME.to_owned(),
            description: "Validates test files".to_owned(),
            transport: McpTransport::Stdio {
                command: vec![exe],
                env: server_env,
            },
            startup_timeout: Duration::from_secs(30),
            timeout: Duration::from_secs(30),
            command_name: SERVER_NAME.to_owned(),
        }],
        notices: Vec::new(),
    };

    // The production exec target: the rhema binary, exactly what
    // `wire` passes in a running frontend (`current_exe`), pinned
    // here because the test process is this binary instead.
    let bin = Path::new(env!("CARGO_BIN_EXE_rhema"));
    let wiring = rhema_mcp::wire::wire_with_bin(
        &mcp_config,
        &BTreeSet::new(),
        session.path(),
        "wire-turn-test",
        std::env::var_os("BASH_ENV"),
        bin,
    )
    .map_err(|e| e.to_string())?;
    // The stubs' `$RHEMA_MCP_BIN`: provision names the running binary
    // in place, the same shape the other wirings use, so the value is
    // the source path and no copy lands in the provision dir.
    let wired = wiring
        .shell_env
        .get("RHEMA_MCP_BIN")
        .ok_or("the env map has no RHEMA_MCP_BIN")?;
    if Path::new(wired.as_str()) != bin {
        return Err(format!(
            "the stubs must exec the real binary, wired to {wired:?} not {bin:?}"
        ));
    }

    // The scripted model: the discovery story, then the turn's end.
    // Boundaries are request arrivals, so each sample of the spawn
    // marker is deterministic.
    let provider = Arc::new(Scripted::new(
        vec![
            bash_call("c1", "mcp servers"),
            bash_call("c2", &format!("{SERVER_NAME} --help")),
            bash_call("c3", &format!("{SERVER_NAME} validate-test --help")),
            bash_call(
                "c4",
                &format!("{SERVER_NAME} validate-test --path /tmp/fake-file.txt"),
            ),
            end_turn("all checks passed"),
        ],
        &marker,
    ));

    // The agent builds its own ShellTools over the wiring's env map,
    // the production shape, `BASH_ENV` and the bridge token included.
    let mut config = HostAgentConfig::new(
        provider.clone(),
        "mock-model",
        workspace.path(),
        rhema_host_agent::AllowAll,
    );
    config.skills = Some(Vec::new());
    config.transcript = None;
    config.shell_env = wiring.shell_env.clone();
    config.mcp_servers = wiring.lines.clone();
    let mut agent = HostAgent::new(config).map_err(|e| e.to_string())?;

    let output = agent
        .turn("validate the fake file", &mut NullSink)
        .await
        .map_err(|e| e.to_string())?;
    let final_text = output
        .message
        .content
        .iter()
        .find_map(|c| match c {
            Content::Text(text) => Some(text.as_str()),
            _ => None,
        })
        .unwrap_or("");
    if final_text != "all checks passed" {
        return Err(format!("unexpected final output: {final_text:?}"));
    }

    // The prompt face: the section lists the server with its command
    // name and description, teaches the discovery story, and the
    // tools array grew by zero, `validate-test` is a bash command,
    // never an advertised tool.
    let requests = provider.requests();
    if requests.len() != 5 {
        return Err(format!("expected 5 requests, got {}", requests.len()));
    }
    let system = requests[0]
        .system
        .as_deref()
        .ok_or("the request carried no system prompt")?;
    if !system.contains("## MCP servers") {
        return Err("the mcp section is missing from the system prompt".to_owned());
    }
    let line = format!("- {SERVER_NAME} (server: {SERVER_NAME}): Validates test files");
    if !system.contains(&line) {
        return Err(format!("the server line is missing: {line}"));
    }
    if !system.contains(&format!("{SERVER_NAME} --help")) {
        return Err("the --help story is missing from the system prompt".to_owned());
    }
    if requests[0].tool_names.as_slice() != ["bash"] {
        return Err(format!(
            "the tools array grew: {:?}",
            requests[0].tool_names
        ));
    }

    // The lifecycle face: `mcp servers` reports without spawning, the
    // `--help` round spawns the one child, the call rides the live
    // connection. Samples sit at request boundaries.
    let spawns: Vec<usize> = requests.iter().map(|r| r.spawns).collect();
    if spawns != [0, 0, 1, 1, 1] {
        return Err(format!(
            "wrong spawn sequence {spawns:?}, want [0, 0, 1, 1, 1]"
        ));
    }

    // The history face: every command's output landed as a tool
    // result, exit code clean.
    let history = &agent.session().history;
    let result_for = |id: &str| -> Result<ToolResult, String> {
        history
            .iter()
            .find_map(|message| {
                message.content.iter().find_map(|content| match content {
                    Content::ToolResult(result) if result.id == id => Some(result.clone()),
                    _ => None,
                })
            })
            .ok_or_else(|| format!("no tool result for {id} in {history:?}"))
    };
    for id in ["c1", "c2", "c3", "c4"] {
        let result = result_for(id)?;
        if result.is_error || result.code != Some(0) {
            return Err(format!(
                "{id} failed: {:?} code {:?} content {}",
                result.is_error, result.code, result.content
            ));
        }
    }
    let servers = result_for("c1")?;
    for needle in ["\"validate-test-mcp\"", "\"idle\"", "Validates test files"] {
        if !servers.content.contains(needle) {
            return Err(format!(
                "`mcp servers` output lacks {needle}: {}",
                servers.content
            ));
        }
    }
    // The server's help lists its tools; the tool's help lists its
    // flags, the two levels the prompt teaches.
    let server_help = result_for("c2")?;
    for needle in ["validate-test", "Validate a test file"] {
        if !server_help.content.contains(needle) {
            return Err(format!(
                "server `--help` output lacks {needle}: {}",
                server_help.content
            ));
        }
    }
    let tool_help = result_for("c3")?;
    for needle in ["--path", "File to validate", "(required)"] {
        if !tool_help.content.contains(needle) {
            return Err(format!(
                "tool `--help` output lacks {needle}: {}",
                tool_help.content
            ));
        }
    }
    let call = result_for("c4")?;
    if !call.content.contains("validation ok: /tmp/fake-file.txt") {
        return Err(format!("the call result lost the echo: {}", call.content));
    }

    // The turn closed with the model's own words.
    let last = history.last().ok_or("empty history")?;
    if last.role != Role::Assistant
        || !last
            .content
            .iter()
            .any(|c| matches!(c, Content::Text(text) if text == "all checks passed"))
    {
        return Err(format!("the turn did not close cleanly: {last:?}"));
    }
    Ok(())
}
