//! The wire seam's tests: filter-before-spawn, the env map's keys,
//! the prompt lines, and one CLI round trip through the provisioned
//! env — over the real bridge and a fake stdio child.

use super::*;
use std::collections::BTreeMap;

use crate::cli;
use rhema_config::mcp::{McpServer, McpTransport};
use std::time::Duration;

/// One stdio server whose child is the fake runner: it answers the
/// handshake and the catalog with `counter`'s shapes (the same
/// shapes the integration suite's runner writes), keyed by
/// `FAKE=scenario` in the env.
fn counter_server(name: &str, command_name: Option<&str>) -> McpServer {
    McpServer {
        name: name.to_owned(),
        description: "a counter".to_owned(),
        transport: McpTransport::Stdio {
            command: vec![
                std::env::current_exe()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned(),
            ],
            env: BTreeMap::new(),
        },
        startup_timeout: Duration::from_secs(10),
        timeout: Duration::from_secs(10),
        command_name: command_name.unwrap_or(name).to_owned(),
    }
}

/// The current test binary re-execs as the fake runner when
/// `RHEMA_FAKE` is set (the integration suite's contract); the lib
/// tests get the same treatment through the runner binary registered
/// in `tests/fake_server.rs`. For the lib-test path, a tiny inline
/// server script suffices: bash printing canned JSON lines.
fn fake_command_server(name: &str) -> McpServer {
    // The child speaks one JSON-RPC line per stdin line, enough for
    // initialize, initialized, tools/list, and tools/call.
    // The ids echo the request's own: the client matches by id, so a
    // canned one never lands.
    let script = r#"
while IFS= read -r line; do
  id=$(printf '%s' "$line" | jq -r '.id // empty')
  case "$line" in
    *'"initialize"'*)
      echo "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"protocolVersion\":\"2025-06-18\",\"capabilities\":{},\"serverInfo\":{\"name\":\"t\",\"version\":\"1\"}}}"
      echo '{"jsonrpc":"2.0","method":"notifications/initialized"}'
      ;;
    *'"tools/list"'*)
      echo "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"tools\":[{\"name\":\"get\",\"description\":\"get the count\",\"inputSchema\":{\"type\":\"object\",\"properties\":{}}}]}}"
      ;;
    *'"tools/call"'*)
      echo "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"content\":[{\"type\":\"text\",\"text\":\"wired\"}],\"isError\":false}}"
      ;;
  esac
done
"#;
    McpServer {
        name: name.to_owned(),
        description: "an inline fake".to_owned(),
        transport: McpTransport::Stdio {
            command: vec!["bash".to_owned(), "-c".to_owned(), script.to_owned()],
            env: BTreeMap::new(),
        },
        startup_timeout: Duration::from_secs(10),
        timeout: Duration::from_secs(10),
        command_name: name.to_owned(),
    }
}

fn config(servers: Vec<McpServer>) -> McpConfig {
    McpConfig {
        servers,
        notices: Vec::new(),
    }
}

#[tokio::test]
async fn wiring_produces_env_lines_and_a_live_bridge() {
    let session = tempfile::tempdir().unwrap();
    let config = config(vec![
        counter_server("atlassian", Some("jira")),
        fake_command_server("counter"),
    ]);
    let mut disabled = BTreeSet::new();
    disabled.insert("atlassian".to_owned());

    let mut wiring = wire(&config, &disabled, session.path(), "wire-env", None)
        .await
        .unwrap();

    // The disabled server left no trace: no line, no stub, no env var
    // pointing at a spawn.
    assert_eq!(wiring.lines.len(), 1);
    assert_eq!(wiring.lines[0].name, "counter");
    assert_eq!(wiring.lines[0].description, "an inline fake");
    let init = wiring
        .shell_env
        .get(provision::VAR_BASH_ENV)
        .map(PathBuf::from)
        .unwrap();
    let text = std::fs::read_to_string(&init).unwrap();
    assert!(text.contains("counter() {"), "{text}");
    assert!(
        !text.contains("atlassian() {"),
        "the disabled stub is gone: {text}"
    );
    assert_eq!(
        wiring
            .shell_env
            .get(provision::VAR_BRIDGE)
            .map(String::as_str),
        Some(wiring.socket().to_string_lossy().as_ref())
    );
    assert!(wiring.shell_env.contains_key(provision::VAR_TOKEN));
    assert!(wiring.socket().exists(), "the bridge bound the socket");

    // One CLI round trip through the provisioned env, bridge live:
    // the mcp meta surface answers. The CLI is synchronous, so it
    // runs on the blocking pool — a blocking read on a worker thread
    // would starve this runtime's accept loop.
    let socket = wiring.socket().to_path_buf();
    let token = wiring.shell_env.get(provision::VAR_TOKEN).unwrap().clone();
    let tmp = session.path().to_path_buf();
    let code = tokio::task::spawn_blocking(move || {
        cli::run_with_env(
            &socket,
            &token,
            Some(&tmp),
            &["mcp", "tools", "counter"]
                .iter()
                .map(std::ffi::OsString::from)
                .collect::<Vec<_>>(),
        )
    })
    .await
    .unwrap();
    assert_eq!(code, 0);

    wiring.shutdown_bridge();
}

#[tokio::test]
async fn the_generated_command_mode_round_trips_through_the_bridge() {
    let session = tempfile::tempdir().unwrap();
    let config = config(vec![fake_command_server("counter")]);
    let mut wiring = wire(&config, &BTreeSet::new(), session.path(), "wire-call", None)
        .await
        .unwrap();

    // __server counter: the catalog, the clap tree, the call.
    let socket = wiring.socket().to_path_buf();
    let token = wiring.shell_env.get(provision::VAR_TOKEN).unwrap().clone();
    let tmp = session.path().to_path_buf();
    let code = tokio::task::spawn_blocking(move || {
        cli::run_with_env(
            &socket,
            &token,
            Some(&tmp),
            &["__server", "counter", "get"]
                .iter()
                .map(std::ffi::OsString::from)
                .collect::<Vec<_>>(),
        )
    })
    .await
    .unwrap();
    assert_eq!(code, 0, "the call exits ok");

    wiring.shutdown_bridge();
}

#[tokio::test]
async fn an_inherited_bash_env_survives_the_map() {
    let session = tempfile::tempdir().unwrap();
    let mut wiring = wire(
        &config(vec![fake_command_server("counter")]),
        &BTreeSet::new(),
        session.path(),
        "wire-inh",
        Some("/etc/profile.d/agent.sh".into()),
    )
    .await
    .unwrap();
    assert_eq!(
        wiring
            .shell_env
            .get(provision::VAR_INHERITED)
            .map(String::as_str),
        Some("/etc/profile.d/agent.sh")
    );
    wiring.shutdown_bridge();
}

#[tokio::test]
async fn reserved_function_names_surface_as_notices() {
    let session = tempfile::tempdir().unwrap();
    let mut server = fake_command_server("meta-server");
    server.command_name = "mcp".to_owned();
    let mut wiring = wire(
        &config(vec![server]),
        &BTreeSet::new(),
        session.path(),
        "wire-meta",
        None,
    )
    .await
    .unwrap();
    assert_eq!(wiring.notices.len(), 1);
    assert!(wiring.notices[0].contains("reserved"));
    wiring.shutdown_bridge();
}
