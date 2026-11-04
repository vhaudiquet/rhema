//! Tool selection: which tools become subcommands, the bare
//! invocation list, and the errors `args` reports.

mod common;

use common::{mapped, overridden, parse, run, tool};
use mcp_to_cli::{MappingError, args, cli_safe, command, list_json};
use serde_json::json;

#[test]
fn cli_safe_accepts_argv_clean_names() {
    for name in [
        "create_issue",
        "a",
        "A9",
        "with-dash",
        "with_underscore",
        "9",
        "help",
    ] {
        assert!(cli_safe(name), "{name} should be safe");
    }
}

#[test]
fn cli_safe_rejects_everything_else() {
    for name in [
        "",
        "has space",
        "dot.name",
        "sla/sh",
        "uni\u{e7}ode",
        "star*",
        "tab\tname",
        "query?",
    ] {
        assert!(!cli_safe(name), "{name} should not be safe");
    }
}

#[test]
fn unsafe_named_tools_are_omitted() {
    let cmd = command(
        "srv",
        &[
            tool(
                "safe",
                "safe tool",
                json!({"type": "object", "properties": {}}),
            ),
            tool(
                "bad name",
                "unreachable",
                json!({"type": "object", "properties": {}}),
            ),
        ],
    );
    assert!(cmd.find_subcommand("safe").is_some());
    assert!(cmd.find_subcommand("bad name").is_none());
}

#[test]
fn help_named_tools_are_skipped() {
    let cmd = command(
        "srv",
        &[
            tool(
                "help",
                "clap owns this name",
                json!({"type": "object", "properties": {}}),
            ),
            tool("other", "fine", json!({"type": "object", "properties": {}})),
        ],
    );
    // cli_safe("help") is true — the skip is the command's, not the
    // charset's.
    assert!(cli_safe("help"));
    assert!(cmd.find_subcommand("help").is_none());
    assert!(cmd.find_subcommand("other").is_some());
}

#[test]
fn non_object_roots_are_omitted() {
    let cmd = command(
        "srv",
        &[
            tool(
                "arr",
                "array root",
                json!({"type": "array", "items": {"type": "string"}}),
            ),
            tool("str", "string root", json!({"type": "string"})),
            tool("null_root", "null root", serde_json::Value::Null),
            tool(
                "object",
                "object root",
                json!({"type": "object", "properties": {}}),
            ),
        ],
    );
    assert!(cmd.find_subcommand("arr").is_none());
    assert!(cmd.find_subcommand("str").is_none());
    assert!(cmd.find_subcommand("null_root").is_none());
    assert!(cmd.find_subcommand("object").is_some());
}

#[test]
fn duplicate_tool_names_keep_the_first() {
    let cmd = command(
        "srv",
        &[
            tool(
                "dup",
                "first wins",
                json!({"type": "object", "properties": {}}),
            ),
            tool(
                "dup",
                "second ignored",
                json!({"type": "object", "properties": {}}),
            ),
        ],
    );
    let sc = cmd.find_subcommand("dup").expect("one subcommand");
    assert_eq!(
        sc.get_about().map(|a| a.to_string()),
        Some("first wins".to_owned())
    );
}

#[test]
fn subcommands_follow_catalog_order() {
    let empty = json!({"type": "object", "properties": {}});
    let cmd = command(
        "srv",
        &[
            tool("bravo", "b", empty.clone()),
            tool("alpha", "a", empty.clone()),
            tool("charlie", "c", empty),
        ],
    );
    let names: Vec<&str> = cmd.get_subcommands().map(|sc| sc.get_name()).collect();
    assert_eq!(names, ["bravo", "alpha", "charlie"]);
}

#[test]
fn descriptions_flow_into_about_and_flag_help_verbatim() {
    let about = "List things, possibly at length, always helpfully";
    let flag_help = "Repository owner (username or organization)";
    let cmd = command(
        "srv",
        &[tool(
            "t",
            about,
            json!({"type": "object", "properties": {"owner": {"type": "string", "description": flag_help}}}),
        )],
    );
    let sc = cmd.find_subcommand("t").expect("tool subcommand");
    assert_eq!(
        sc.get_about().map(|a| a.to_string()),
        Some(about.to_owned())
    );
    let arg = sc
        .get_arguments()
        .find(|a| a.get_long() == Some("owner"))
        .expect("owner flag");
    assert_eq!(
        arg.get_help().map(|h| h.to_string()),
        Some(flag_help.to_owned())
    );
}

#[test]
fn bare_invocation_matches_and_args_reports_it() {
    let cmd = command(
        "srv",
        &[tool("t", "d", json!({"type": "object", "properties": {}}))],
    );
    let matches = parse(&cmd, &[]).expect("bare invocation parses");
    assert!(matches.subcommand().is_none());
    assert_eq!(
        args(&cmd, &matches).unwrap_err(),
        MappingError::NoSubcommand
    );
}

#[test]
fn unknown_tool_subcommand_is_claps_error() {
    let cmd = command(
        "srv",
        &[tool("t", "d", json!({"type": "object", "properties": {}}))],
    );
    assert!(parse(&cmd, &["nope"]).is_none());
}

#[test]
fn matches_outside_the_command_report_unknown_subcommand() {
    let with_a = command(
        "srv",
        &[tool("a", "d", json!({"type": "object", "properties": {}}))],
    );
    let empty = command("srv", &[]);
    let matches = parse(&with_a, &["a"]).expect("parses");
    assert_eq!(
        args(&empty, &matches).unwrap_err(),
        MappingError::UnknownSubcommand {
            name: "a".to_owned(),
        }
    );
}

#[test]
fn list_json_is_name_and_description_pairs() {
    let value = list_json(&[
        tool("create_issue", "Create an issue", json!({"type": "object"})),
        tool(
            "get_me",
            "Get the authenticated user",
            json!({"type": "object"}),
        ),
    ]);
    assert_eq!(
        value,
        json!([
            {"name": "create_issue", "description": "Create an issue"},
            {"name": "get_me", "description": "Get the authenticated user"},
        ])
    );
}

#[test]
fn list_json_of_an_empty_catalog_is_an_empty_array() {
    assert_eq!(list_json(&[]), json!([]));
}

#[test]
fn list_json_mirrors_the_rendered_tree() {
    let catalog = [
        tool("safe", "kept", json!({"type": "object", "properties": {}})),
        tool(
            "bad name",
            "dropped",
            json!({"type": "object", "properties": {}}),
        ),
        tool(
            "help",
            "dropped",
            json!({"type": "object", "properties": {}}),
        ),
        tool("str_root", "dropped", json!({"type": "string"})),
        tool(
            "safe",
            "duplicate dropped",
            json!({"type": "object", "properties": {}}),
        ),
    ];
    let cmd = command("srv", &catalog);
    let tree: Vec<serde_json::Value> = cmd
        .get_subcommands()
        .map(|sc| {
            json!({
                "name": sc.get_name(),
                "description": sc.get_about().map(|a| a.to_string()).unwrap_or_default(),
            })
        })
        .collect();
    assert_eq!(list_json(&catalog), serde_json::Value::Array(tree));
    assert_eq!(
        list_json(&catalog),
        json!([{"name": "safe", "description": "kept"}])
    );
}

#[test]
fn mapped_and_overridden_cover_the_whole_surface() {
    let cmd = command(
        "srv",
        &[tool(
            "t",
            "d",
            json!({"type": "object", "properties": {"owner": {"type": "string"}}}),
        )],
    );
    assert_eq!(overridden(run(&cmd, &["t", "--json", "{}"]).unwrap()), "{}");
    assert_eq!(
        mapped(run(&cmd, &["t", "--owner", "o"]).unwrap()),
        json!({"owner": "o"})
    );
}
