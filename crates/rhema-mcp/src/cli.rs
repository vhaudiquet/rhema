//! The CLI: the `rhema mcp` meta verbs and the `__server` generated
//! command mode — everything a bash command spawned by an agent sees.
//!
//! Two surfaces, one JSON-on-stdout rule, distinct exit codes (spec
//! §5): `0` ok; `2` usage (bad flags, unknown tool, unparseable or
//! oversize args JSON); `3` unknown server; `4` transport (bridge
//! unreachable, server down past respawn); `5` the server reported
//! failure (JSON-RPC error or `isError: true` — the failure envelope
//! still prints, it is the model's diagnostic); `6` timeout; `1`
//! reserved for internal faults and never printed by design.
//!
//! The bridge client is a hand-rolled HTTP/1.1 exchange over the
//! Unix socket, `Connection: close` — one request per process, no
//! connection pool to grow. The `/call` answer arrives as the
//! bridge's SSE stream; keepalive comments are skipped, the first
//! terminal event carries the outcome.

use std::io::Read as _;
use std::path::{Path, PathBuf};

use base64::Engine as _;
use mcp_to_cli::{Payload, ToolInfo, args as map_args, command as build_command, list_json};
use serde_json::{Value, json};

use crate::protocol::ToolDef;

/// The invocation's outcome: an exit code with a name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exit {
    /// The call worked, or the help text was the answer.
    Ok,
    /// Usage: clap rejected the flags, or the args JSON was bad.
    Usage,
    /// The server name is not in config.
    UnknownServer,
    /// Transport: bridge unreachable, spawn failed, cooldown.
    Transport,
    /// The server reported failure; the envelope printed.
    ServerReported,
    /// The call timed out inside the budget.
    Timeout,
}

impl From<Exit> for i32 {
    fn from(exit: Exit) -> i32 {
        match exit {
            Exit::Ok => 0,
            Exit::Usage => 2,
            Exit::UnknownServer => 3,
            Exit::Transport => 4,
            Exit::ServerReported => 5,
            Exit::Timeout => 6,
        }
    }
}

/// Why the process ends now: either a named exit or an internal
/// fault, which exits `1` and prints nothing (spec §5).
enum Outcome {
    Exit(Exit),
    Internal(String),
}

/// The environment the init file provides; any missing piece means
/// the CLI ran outside a provisioned shell.
struct CliEnv {
    socket: PathBuf,
    token: String,
    tmp: Option<PathBuf>,
}

/// The environment the init file provides, read from this process's
/// env; any missing piece means the CLI ran outside a provisioned
/// shell.
fn cli_env() -> Result<CliEnv, Outcome> {
    let socket = std::env::var_os(crate::provision::VAR_BRIDGE)
        .map(PathBuf::from)
        .ok_or_else(|| Outcome::Internal("missing RHEMA_MCP_BRIDGE".to_owned()))?;
    let token = std::env::var(crate::provision::VAR_TOKEN)
        .map_err(|_| Outcome::Internal("missing RHEMA_MCP_TOKEN".to_owned()))?;
    let tmp = std::env::var_os(crate::provision::VAR_TMP).map(PathBuf::from);
    Ok(CliEnv { socket, token, tmp })
}

/// Runs the CLI with process-style argv (first element is the mode:
/// `mcp` or `__server`) and the env vars the init file sets. The
/// caller's binary dispatches here; the return is the process exit
/// code.
#[must_use]
pub fn run(argv: &[std::ffi::OsString]) -> i32 {
    match dispatch(cli_env(), argv) {
        Ok(exit) => i32::from(exit),
        Err(Outcome::Exit(exit)) => i32::from(exit),
        Err(Outcome::Internal(reason)) => {
            let _ = reason; // never printed, by design
            i32::from(Exit::Transport)
        }
    }
}

/// Runs the CLI over an explicit environment — the seam the
/// integration tests use, since env vars are process-global and
/// parallel tests would race on them. Process runs go through
/// [`run`], which reads the init file's vars.
///
/// # Panics
///
/// Never: the only difference from [`run`] is where the env comes
/// from.
pub fn run_with_env(
    socket: &Path,
    token: &str,
    tmp: Option<&Path>,
    argv: &[std::ffi::OsString],
) -> i32 {
    let env = CliEnv {
        socket: socket.to_path_buf(),
        token: token.to_owned(),
        tmp: tmp.map(Path::to_path_buf),
    };
    match dispatch(Ok(env), argv) {
        Ok(exit) => i32::from(exit),
        Err(Outcome::Exit(exit)) => i32::from(exit),
        Err(Outcome::Internal(reason)) => {
            let _ = reason; // never printed, by design
            i32::from(Exit::Transport)
        }
    }
}

/// Mode dispatch over an explicit env: `run` feeds the process env,
/// tests feed a constructed one — env vars are process-global and
/// parallel tests would race on them.
fn dispatch(env: Result<CliEnv, Outcome>, argv: &[std::ffi::OsString]) -> Result<Exit, Outcome> {
    let Some(mode) = argv.first().and_then(|a| a.to_str()) else {
        return Ok(Exit::Transport); // no argv at all: unusable
    };
    let rest = &argv[1.min(argv.len())..];
    match mode {
        "__server" => server_mode(&env?, rest),
        "mcp" => meta_mode(&env?, rest),
        other => {
            eprintln!("unknown mode `{other}`; expected `mcp` or `__server`");
            Ok(Exit::Usage)
        }
    }
}

// ---------------------------------------------------------------------
// The bridge client
// ---------------------------------------------------------------------

/// One request's answer: status and the body (already dechunked).
struct Answer {
    status: u16,
    body: String,
}

/// Performs one HTTP/1.1 exchange over the bridge socket.
fn exchange(
    env: &CliEnv,
    method: &str,
    path: &str,
    body: Option<&str>,
) -> Result<Answer, std::io::Error> {
    let mut stream = std::os::unix::net::UnixStream::connect(&env.socket)?;
    stream.set_read_timeout(Some(std::time::Duration::from_secs(180)))?;
    let mut request = format!(
        "{method} {path} HTTP/1.1\r\nHost: rhema-mcp\r\nAuthorization: Bearer {}\r\nConnection: close\r\n",
        env.token
    );
    match body {
        Some(body) => {
            request.push_str("Content-Type: application/json\r\n");
            request.push_str(&format!("Content-Length: {}\r\n\r\n", body.len()));
            request.push_str(body);
        }
        None => request.push_str("\r\n"),
    }
    use std::io::Write as _;
    stream.write_all(request.as_bytes())?;
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw)?;
    parse_answer(&raw)
}

/// Splits head from body, decodes chunked transfer or content-length.
fn parse_answer(raw: &[u8]) -> Result<Answer, std::io::Error> {
    let split = find_subslice(raw, b"\r\n\r\n")
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidData, "no header end"))?;
    let head = String::from_utf8_lossy(&raw[..split]);
    let mut status = 0;
    let mut chunked = false;
    let mut content_length: Option<usize> = None;
    for (index, line) in head.lines().enumerate() {
        let lower = line.to_ascii_lowercase();
        if index == 0 {
            status = lower
                .split_whitespace()
                .nth(1)
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);
        } else if let Some(value) = lower.strip_prefix("transfer-encoding:") {
            chunked = value.trim() == "chunked";
        } else if let Some(value) = lower.strip_prefix("content-length:") {
            content_length = value.trim().parse().ok();
        }
    }
    let body_bytes = &raw[split + 4..];
    let body = if chunked {
        dechunk(body_bytes)?
    } else if let Some(length) = content_length {
        body_bytes
            .get(..length)
            .map(<[u8]>::to_vec)
            .unwrap_or_else(|| body_bytes.to_vec())
    } else {
        body_bytes.to_vec()
    };
    Ok(Answer {
        status,
        body: String::from_utf8_lossy(&body).into_owned(),
    })
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// Decodes a chunked body: `size\r\n<data>\r\n` runs, `0\r\n\r\n` ends.
fn dechunk(bytes: &[u8]) -> Result<Vec<u8>, std::io::Error> {
    let mut out = Vec::new();
    let mut rest = bytes;
    loop {
        let Some(line_end) = find_subslice(rest, b"\r\n") else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "chunk size line missing",
            ));
        };
        let size_text = String::from_utf8_lossy(&rest[..line_end]);
        let size = usize::from_str_radix(size_text.trim().split(';').next().unwrap_or(""), 16)
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidData, "bad chunk size"))?;
        rest = &rest[line_end + 2..];
        if size == 0 {
            return Ok(out);
        }
        if rest.len() < size {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "truncated chunk",
            ));
        }
        out.extend_from_slice(&rest[..size]);
        rest = &rest[size..];
        if rest.starts_with(b"\r\n") {
            rest = &rest[2..];
        }
    }
}

/// One SSE terminal event: `(event name, data)`. Keepalive comments
/// are skipped; the first event-carrying frame wins.
fn sse_terminal(body: &str) -> Option<(String, String)> {
    for frame in body.split("\r\n\r\n") {
        let mut event = String::new();
        let mut data = Vec::new();
        for line in frame.lines() {
            if let Some(name) = line.strip_prefix("event: ") {
                event = name.trim().to_owned();
            } else if let Some(payload) = line.strip_prefix("data: ") {
                data.push(payload.trim().to_owned());
            }
        }
        if !event.is_empty() {
            return Some((event, data.join("\n")));
        }
    }
    None
}

/// POSTs `/call` and reads the stream to its terminal event.
fn call_over_bridge(
    env: &CliEnv,
    server: &str,
    tool: &str,
    arguments: &Value,
) -> Result<(String, String), Fault> {
    let body = json!({"server": server, "tool": tool, "arguments": arguments}).to_string();
    let answer =
        exchange(env, "POST", "/call", Some(&body)).map_err(|e| Fault::transport(e.to_string()))?;
    match answer.status {
        200 => {}
        401 => {
            return Err(Fault::transport("the bridge refused the token"));
        }
        413 => {
            return Err(Fault::usage(
                "the arguments exceed the bridge's 1 MiB body cap; ride stdin (`--json -`)",
            ));
        }
        other => {
            return Err(Fault::transport(format!("bridge answered HTTP {other}")));
        }
    }
    sse_terminal(&answer.body)
        .ok_or_else(|| Fault::transport("the call stream ended without a terminal event"))
}

/// What went wrong before any envelope could exist: an exit code with
/// a stderr line.
#[derive(Debug)]
struct Fault {
    exit: Exit,
    message: String,
}

impl Fault {
    fn transport(message: impl Into<String>) -> Self {
        Self {
            exit: Exit::Transport,
            message: message.into(),
        }
    }

    fn usage(message: impl Into<String>) -> Self {
        Self {
            exit: Exit::Usage,
            message: message.into(),
        }
    }

    /// Emits the message where the spec allows one and yields the
    /// exit.
    fn report(self) -> Exit {
        eprintln!("{}", self.message);
        self.exit
    }
}

// ---------------------------------------------------------------------
// Envelope: media substitution and printing
// ---------------------------------------------------------------------

/// The extension a media block may write, by mime type; anything else
/// becomes a text placeholder with the byte count. The file name is
/// always our own `<sha256>.<ext>` — no server-provided name is ever
/// used, so no traversal is possible by construction.
fn media_ext(mime: &str) -> Option<&'static str> {
    match mime {
        "image/png" => Some("png"),
        "image/jpeg" | "image/jpg" => Some("jpg"),
        "image/gif" => Some("gif"),
        "image/webp" => Some("webp"),
        "audio/wav" | "audio/x-wav" => Some("wav"),
        "audio/mpeg" | "audio/mp3" => Some("mp3"),
        "audio/ogg" => Some("ogg"),
        _ => None,
    }
}

/// The sha256 hex digest of `bytes`, lowercase.
fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::Digest as _;
    let digest = sha2::Sha256::digest(bytes);
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Substitutes media blocks with file paths under the session temp
/// dir; everything else passes through verbatim.
fn substitute_media(envelope: &mut Value, env: &CliEnv) {
    let Some(content) = envelope.get_mut("content").and_then(Value::as_array_mut) else {
        return;
    };
    for block in content.iter_mut() {
        let kind = block.get("type").and_then(Value::as_str).unwrap_or("");
        if kind != "image" && kind != "audio" {
            continue;
        }
        let mime = block.get("mimeType").and_then(Value::as_str).unwrap_or("");
        let data = block.get("data").and_then(Value::as_str).unwrap_or("");
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(data)
            .unwrap_or_default();
        let count = bytes.len();
        let written = env.tmp.as_ref().and_then(|tmp| {
            let ext = media_ext(mime)?;
            let dir = tmp.join("media");
            std::fs::create_dir_all(&dir).ok()?;
            let path = dir.join(format!("{}.{ext}", sha256_hex(&bytes)));
            std::fs::write(&path, &bytes).ok()?;
            Some(path)
        });
        *block = match written {
            Some(path) => json!({
                "type": kind,
                "path": path.to_string_lossy(),
                "mimeType": mime,
                "bytes": count,
            }),
            None => json!({
                "type": "text",
                "text": format!("[{kind} ({mime}) omitted: {count} bytes]"),
            }),
        };
    }
}

/// Prints one terminal event and derives the exit: a `tool_error`
/// envelope prints and reports `ServerReported`; a manager `error`
/// event maps by kind; a `result` prints and reports ok.
fn print_envelope(event: &str, data: &str, env: &CliEnv) -> Exit {
    let Ok(mut value) = serde_json::from_str::<Value>(data) else {
        return Exit::Transport; // a stream that lied about its own format
    };
    if event == "error" {
        // Manager error: kind is the exit-code mapping.
        let kind = value
            .get("kind")
            .and_then(Value::as_str)
            .unwrap_or("transport");
        let message = value
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("the call failed");
        return match kind {
            "unknown_server" => {
                eprintln!("{message}");
                Exit::UnknownServer
            }
            "unknown_tool" => {
                eprintln!("{message}");
                Exit::Usage
            }
            "timeout" => {
                eprintln!("{message}");
                Exit::Timeout
            }
            "rpc" => {
                eprintln!("{message}");
                Exit::ServerReported
            }
            _ => {
                eprintln!("{message}");
                Exit::Transport
            }
        };
    }
    substitute_media(&mut value, env);
    match serde_json::to_string_pretty(&value) {
        Ok(text) => println!("{text}"),
        Err(_) => println!("{data}"),
    }
    if event == "tool_error" {
        Exit::ServerReported
    } else {
        Exit::Ok
    }
}

// ---------------------------------------------------------------------
// The meta verbs
// ---------------------------------------------------------------------

fn meta_mode(env: &CliEnv, argv: &[std::ffi::OsString]) -> Result<Exit, Outcome> {
    let verb = argv.first().and_then(|a| a.to_str()).unwrap_or("");
    let rest: Vec<String> = argv[1.min(argv.len())..]
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    match verb {
        "servers" => {
            let answer = exchange(env, "GET", "/servers", None)
                .map_err(|e| Outcome::Exit(Fault::transport(e.to_string()).report()))?;
            if answer.status != 200 {
                return Err(Outcome::Internal(format!(
                    "bridge answered {}",
                    answer.status
                )));
            }
            println!("{}", answer.body);
            Ok(Exit::Ok)
        }
        "tools" => {
            let Some(server) = rest.first() else {
                eprintln!("usage: mcp tools <server>");
                return Ok(Exit::Usage);
            };
            let path = format!("/tools/{}", percent_encode(server));
            let answer = exchange(env, "GET", &path, None)
                .map_err(|e| Outcome::Exit(Fault::transport(e.to_string()).report()))?;
            if answer.status == 404 {
                eprintln!("unknown server `{server}`");
                return Ok(Exit::UnknownServer);
            }
            if answer.status != 200 {
                return Err(Outcome::Internal(format!(
                    "bridge answered {}",
                    answer.status
                )));
            }
            println!("{}", answer.body);
            Ok(Exit::Ok)
        }
        "schema" => {
            let [server, tool] = rest.as_slice() else {
                eprintln!("usage: mcp schema <server> <tool>");
                return Ok(Exit::Usage);
            };
            let path = format!(
                "/schema/{}/{}",
                percent_encode(server),
                percent_encode(tool)
            );
            let answer = exchange(env, "GET", &path, None)
                .map_err(|e| Outcome::Exit(Fault::transport(e.to_string()).report()))?;
            match answer.status {
                200 => {
                    println!("{}", answer.body);
                    Ok(Exit::Ok)
                }
                404 => {
                    eprintln!("{}", answer.body);
                    Ok(Exit::Usage)
                }
                _ => Err(Outcome::Internal(format!(
                    "bridge answered {}",
                    answer.status
                ))),
            }
        }
        "call" => {
            let [server, tool, rest @ ..] = rest.as_slice() else {
                eprintln!("usage: mcp call <server> <tool> [args-json]");
                return Ok(Exit::Usage);
            };
            let args = match rest.first().map(String::as_str) {
                None | Some("-") => {
                    let mut input = String::new();
                    std::io::stdin()
                        .read_to_string(&mut input)
                        .map_err(|e| Outcome::Internal(e.to_string()))?;
                    parse_args_json(&input).map_err(fault_exit)?
                }
                Some(text) => parse_args_json(text).map_err(fault_exit)?,
            };
            let (event, data) = call_over_bridge(env, server, tool, &args)
                .map_err(|fault| Outcome::Exit(fault.report()))?;
            Ok(print_envelope(&event, &data, env))
        }
        other => {
            eprintln!("unknown verb `{other}`; expected servers, tools, schema, or call");
            Ok(Exit::Usage)
        }
    }
}

/// Parses the args JSON: empty reads as `{}`; anything unparseable is
/// a usage fault.
fn parse_args_json(text: &str) -> Result<Value, Fault> {
    if text.trim().is_empty() {
        return Ok(json!({}));
    }
    serde_json::from_str(text).map_err(|e| Fault::usage(format!("args JSON: {e}")))
}

/// Resolves a fault to the caller's Outcome: the message prints where
/// the spec allows one, then the exit code carries the rest.
fn fault_exit(fault: Fault) -> Outcome {
    Outcome::Exit(fault.report())
}

// ---------------------------------------------------------------------
// The generated-command mode
// ---------------------------------------------------------------------

/// `__server <name> [--help] [tool [flags…]] [--json <JSON> | --json -]`
fn server_mode(env: &CliEnv, argv: &[std::ffi::OsString]) -> Result<Exit, Outcome> {
    let Some(server) = argv.first().and_then(|a| a.to_str().map(str::to_owned)) else {
        eprintln!("usage: __server <name> [tool [flags…]]");
        return Ok(Exit::Usage);
    };
    let rest: Vec<String> = argv[1.min(argv.len())..]
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();

    // Fetch the catalog, build the tree, parse, call.
    let path = format!("/catalog/{}", percent_encode(&server));
    let answer = exchange(env, "GET", &path, None)
        .map_err(|e| Outcome::Exit(Fault::transport(e.to_string()).report()))?;
    match answer.status {
        200 => {}
        404 => {
            eprintln!("unknown server `{server}`");
            return Ok(Exit::UnknownServer);
        }
        other => return Err(Outcome::Internal(format!("bridge answered {other}"))),
    }
    let Ok(catalog) = serde_json::from_str::<Vec<ToolDef>>(&answer.body) else {
        return Err(Outcome::Internal(
            "the catalog was not a tool list".to_owned(),
        ));
    };
    let infos: Vec<ToolInfo> = catalog
        .iter()
        .map(|tool| ToolInfo {
            name: tool.name.clone(),
            description: tool.description.clone().unwrap_or_default(),
            input_schema: tool.input_schema.clone(),
        })
        .collect();
    let mut tree = build_command(&server, &infos);
    let mut argv_for_clap = vec![std::ffi::OsString::from(&server)];
    argv_for_clap.extend(rest.iter().map(Into::into));
    let matches = match tree.try_get_matches_from_mut(argv_for_clap) {
        Ok(matches) => matches,
        Err(err) => {
            // clap's own error text, self-teaching where a JSON parse
            // error is not. A `--help` prints to stdout and exits ok.
            let _ = err.print();
            return Ok(if err.use_stderr() {
                Exit::Usage
            } else {
                Exit::Ok
            });
        }
    };
    if matches.subcommand().is_none() {
        // Bare invocation: list the tools, per the crate's contract.
        println!(
            "{}",
            serde_json::to_string_pretty(&list_json(&infos)).unwrap_or_else(|_| "[]".to_owned())
        );
        return Ok(Exit::Ok);
    }
    let payload = match map_args(&tree, &matches) {
        Ok(payload) => payload,
        Err(err) => {
            eprintln!("{err}");
            return Ok(Exit::Usage);
        }
    };
    let Some(tool) = matches.subcommand_name().map(str::to_owned) else {
        return Err(Outcome::Internal("a parsed subcommand vanished".to_owned()));
    };
    let arguments = match payload {
        // The crate never touches I/O: `-` is the caller's stdin.
        Payload::Overridden(text) if text == "-" => {
            let mut input = String::new();
            std::io::stdin()
                .read_to_string(&mut input)
                .map_err(|e| Outcome::Internal(e.to_string()))?;
            parse_args_json(&input).map_err(fault_exit)?
        }
        Payload::Overridden(text) => serde_json::from_str(&text)
            .map_err(|e| fault_exit(Fault::usage(format!("--json payload: {e}"))))?,
        Payload::Mapped(value) => value,
        // `#[non_exhaustive]`: a future payload kind is a usage
        // error until this CLI grows with the crate.
        _ => {
            return Ok(Exit::Usage);
        }
    };
    let (event, data) = call_over_bridge(env, &server, &tool, &arguments)
        .map_err(|fault| Outcome::Exit(fault.report()))?;
    Ok(print_envelope(&event, &data, env))
}

/// Percent-encodes a path segment: server names are `[a-z0-9_-]+` by
/// config rule, but the CLI defends anyway.
fn percent_encode(segment: &str) -> String {
    let mut out = String::with_capacity(segment.len());
    for byte in segment.bytes() {
        match byte {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char);
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests;
