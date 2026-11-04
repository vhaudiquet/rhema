//! Shared helpers for the mapping matrix: argv driving and payload
//! unwrapping.

use mcp_to_cli::{Payload, ToolInfo, args};

/// A [`ToolInfo`] from parts, for inline schemas.
pub fn tool(name: &str, description: &str, input_schema: serde_json::Value) -> ToolInfo {
    ToolInfo {
        name: name.to_owned(),
        description: description.to_owned(),
        input_schema,
    }
}

/// Parses `argv` (no program name) against `cmd`; `None` is clap
/// rejecting the argv — the usage-error path.
pub fn parse(cmd: &clap::Command, argv: &[&str]) -> Option<clap::ArgMatches> {
    let mut full: Vec<String> = vec!["srv".to_owned()];
    full.extend(argv.iter().map(|a| (*a).to_owned()));
    cmd.clone().try_get_matches_from(full).ok()
}

/// Parses clap-valid `argv` and maps the matched tool.
pub fn run(
    cmd: &clap::Command,
    argv: &[&str],
) -> Result<mcp_to_cli::Payload, mcp_to_cli::MappingError> {
    let matches = parse(cmd, argv).expect("argv parses");
    args(cmd, &matches)
}

/// The mapped object of a payload that must be mapped.
pub fn mapped(payload: Payload) -> serde_json::Value {
    match payload {
        Payload::Mapped(value) => value,
        other => panic!("expected a mapped payload, got {other:?}"),
    }
}

/// The raw string of a payload that must be the `--json` override.
pub fn overridden(payload: Payload) -> String {
    match payload {
        Payload::Overridden(raw) => raw,
        other => panic!("expected an overridden payload, got {other:?}"),
    }
}
