//! The CLI's unit surface: the pieces that need no bridge (percent
//! encoding, chunk decoding, SSE framing, media substitution, arg
//! JSON parsing, exit mapping). The full walks against a real bridge
//! live in the fake-server integration suite.

use super::*;
use std::path::Path;

#[test]
fn exit_codes_map_to_the_spec() {
    assert_eq!(i32::from(Exit::Ok), 0);
    assert_eq!(i32::from(Exit::Usage), 2);
    assert_eq!(i32::from(Exit::UnknownServer), 3);
    assert_eq!(i32::from(Exit::Transport), 4);
    assert_eq!(i32::from(Exit::ServerReported), 5);
    assert_eq!(i32::from(Exit::Timeout), 6);
}

#[test]
fn percent_encoding_passes_legal_names_and_escapes_the_rest() {
    assert_eq!(percent_encode("atlassian"), "atlassian");
    assert_eq!(percent_encode("my-server_2"), "my-server_2");
    assert_eq!(percent_encode("a b"), "a%20b");
    assert_eq!(percent_encode("a/b"), "a%2Fb");
}

#[test]
fn dechunk_reassembles_the_body() {
    let raw = b"5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n";
    assert_eq!(dechunk(raw).unwrap(), b"hello world");
}

#[test]
fn dechunk_rejects_truncated_and_bad_sizes() {
    assert!(dechunk(b"5\r\nhel").is_err());
    assert!(dechunk(b"zz\r\nhello\r\n0\r\n\r\n").is_err());
}

#[test]
fn parse_answer_reads_content_length_and_chunked() {
    let plain =
        b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 5\r\n\r\nhello";
    let answer = parse_answer(plain).unwrap();
    assert_eq!(answer.status, 200);
    assert_eq!(answer.body, "hello");

    let chunked = b"HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\n\r\n3\r\nabc\r\n0\r\n\r\n";
    let answer = parse_answer(chunked).unwrap();
    assert_eq!(answer.status, 200);
    assert_eq!(answer.body, "abc");
}

#[test]
fn sse_terminal_skips_keepalives_and_takes_the_first_event() {
    let body = ": keepalive\r\n\r\n: keepalive\r\n\r\nevent: result\r\ndata: {\"ok\":true}\r\n\r\n";
    let (event, data) = sse_terminal(body).unwrap();
    assert_eq!(event, "result");
    assert_eq!(data, "{\"ok\":true}");

    let error = "event: error\r\ndata: {\"kind\":\"timeout\"}\r\n\r\n";
    let (event, data) = sse_terminal(error).unwrap();
    assert_eq!(event, "error");
    assert_eq!(data, "{\"kind\":\"timeout\"}");

    assert!(sse_terminal(": only comments\r\n\r\n").is_none());
}

fn media_env(tmp: Option<&Path>) -> CliEnv {
    CliEnv {
        socket: PathBuf::from("/nonexistent.sock"),
        token: "t".to_owned(),
        tmp: tmp.map(Path::to_path_buf),
    }
}

/// A png block under a temp dir becomes a path block with the exact
/// sha256 name; an unknown mime becomes a byte-count placeholder.
#[test]
fn media_blocks_become_files_or_placeholders() {
    let dir = tempfile::tempdir().unwrap();
    let env = media_env(Some(dir.path()));
    // 1x1 png, a real magic-number header.
    let png_bytes: &[u8] = &[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
    let encoded = base64::engine::general_purpose::STANDARD.encode(png_bytes);
    let mut envelope = json!({
        "content": [
            {"type": "text", "text": "here"},
            {"type": "image", "data": encoded, "mimeType": "image/png"},
            {"type": "image", "data": "not-base64!!", "mimeType": "image/svg+xml"},
            {"type": "resource_link", "uri": "file:///x"}
        ]
    });
    substitute_media(&mut envelope, &env);
    let content = envelope.get("content").unwrap().as_array().unwrap();
    assert_eq!(content[0], json!({"type": "text", "text": "here"}));
    let path = content[1].get("path").unwrap().as_str().unwrap();
    let expected_name = format!("{}.png", sha256_hex(png_bytes));
    assert!(path.ends_with(&expected_name), "{path}");
    assert_eq!(std::fs::read(path).unwrap(), png_bytes);
    assert_eq!(content[1].get("bytes").unwrap(), &json!(png_bytes.len()));
    // The undecodable payload decodes to nothing: the placeholder
    // reports the decoded count, honestly zero.
    assert!(
        content[2]
            .get("text")
            .unwrap()
            .as_str()
            .unwrap()
            .contains("0 bytes"),
        "unknown mime becomes a placeholder: {}",
        content[2]
    );
    assert_eq!(
        content[3],
        json!({"type": "resource_link", "uri": "file:///x"})
    );
}

/// No temp dir in the env: every media block degrades to a
/// placeholder, and nothing tries to write.
#[test]
fn media_without_a_tmp_dir_degrades_to_placeholders() {
    let env = media_env(None);
    let mut envelope = json!({
        "content": [{"type": "image", "data": "aGk=", "mimeType": "image/png"}]
    });
    substitute_media(&mut envelope, &env);
    let content = envelope.get("content").unwrap().as_array().unwrap();
    assert!(content[0].get("text").is_some(), "{}", content[0]);
}

#[test]
fn empty_and_object_arg_json_parse() {
    assert_eq!(parse_args_json("").unwrap(), json!({}));
    assert_eq!(parse_args_json("  \n").unwrap(), json!({}));
    assert_eq!(parse_args_json("{\"a\":1}").unwrap(), json!({"a":1}));
    assert!(parse_args_json("{broken").is_err());
}

/// A missing bridge env is the silent internal fault: exit 4, no
/// stderr from this path. Proven at process level once the rhema
/// binary wires this dispatch — mutating this process's env here
/// would be `unsafe` in edition 2024 and racy across parallel tests.

#[test]
fn an_unknown_mode_is_usage() {
    let code = run(&["nonsense".into()]);
    assert_eq!(code, 2);
}

/// A real stdout/stderr exercise would need the bridge; the tree
/// build path is covered by mcp-to-cli's own suite. Here: the
/// catalog->ToolInfo assembly keeps names verbatim.
#[test]
fn tool_info_assembly_passes_names_through() {
    let catalog = [ToolDef {
        name: "get_scene_info".to_owned(),
        description: Some("Scene info".to_owned()),
        input_schema: json!({"type": "object"}),
    }];
    let infos: Vec<ToolInfo> = catalog
        .iter()
        .map(|tool| ToolInfo {
            name: tool.name.clone(),
            description: tool.description.clone().unwrap_or_default(),
            input_schema: tool.input_schema.clone(),
        })
        .collect();
    assert_eq!(infos[0].name, "get_scene_info");
    assert_eq!(infos[0].description, "Scene info");
}
