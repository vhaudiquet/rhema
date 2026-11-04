//! The mapping matrix against vendored real schemas: github's
//! github-mcp-server, an Atlassian-shaped stand-in, and a
//! FastMCP-shaped stand-in. See tests/fixtures/README.md.

mod common;

use clap::Command;
use common::{mapped, overridden, parse, run, tool};
use mcp_to_cli::{args, command};
use serde_json::json;

/// The vendored fixture `name`, its JSON as the wire format carried it.
fn fixture(name: &str) -> serde_json::Value {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name);
    let raw = std::fs::read_to_string(&path).expect("fixture is readable");
    serde_json::from_str(&raw).unwrap_or_else(|e| panic!("fixture {name} parses: {e}"))
}

fn catalog(names: &[&str]) -> Command {
    let tools: Vec<mcp_to_cli::ToolInfo> = names
        .iter()
        .map(|name| {
            tool(
                name,
                &format!("the {name} tool"),
                fixture(&format!("{name}.json")),
            )
        })
        .collect();
    command("github", &tools)
}

#[test]
fn create_pull_request_is_the_worked_example() {
    let cmd = catalog(&["create_pull_request"]);
    // The spec's own example, flags verbatim.
    let value = mapped(
        run(
            &cmd,
            &[
                "create_pull_request",
                "--owner",
                "acme",
                "--repo",
                "api",
                "--title",
                "Add /health",
                "--head",
                "feature/health",
                "--base",
                "main",
                "--draft",
                "--reviewers",
                "alice",
                "--reviewers",
                "bob",
            ],
        )
        .unwrap(),
    );
    assert_eq!(
        value,
        json!({
            "owner": "acme",
            "repo": "api",
            "title": "Add /health",
            "head": "feature/health",
            "base": "main",
            "draft": true,
            "reviewers": ["alice", "bob"],
        })
    );
}

#[test]
fn create_pull_request_omitted_optionals_stay_absent() {
    let cmd = catalog(&["create_pull_request"]);
    let value = mapped(
        run(
            &cmd,
            &[
                "create_pull_request",
                "--owner",
                "o",
                "--repo",
                "r",
                "--title",
                "t",
                "--head",
                "h",
                "--base",
                "b",
            ],
        )
        .unwrap(),
    );
    // draft, maintainer_can_modify, body, reviewers: all absent.
    assert_eq!(
        value,
        json!({"owner": "o", "repo": "r", "title": "t", "head": "h", "base": "b"})
    );
    // And the required scalars are enforced.
    assert!(parse(&cmd, &["create_pull_request", "--owner", "o"]).is_none());
}

#[test]
fn search_repositories_bool_default_true_takes_explicit_false() {
    let cmd = catalog(&["search_repositories"]);
    // The spec's example, verbatim.
    let value = mapped(
        run(
            &cmd,
            &[
                "search_repositories",
                "--query",
                "org:acme",
                "--minimal_output",
                "false",
            ],
        )
        .unwrap(),
    );
    assert_eq!(value, json!({"query": "org:acme", "minimal_output": false}));
    // Omitted, the key stays absent and the server applies its true.
    let omitted = mapped(run(&cmd, &["search_repositories", "--query", "q"]).unwrap());
    assert_eq!(omitted, json!({"query": "q"}));
}

#[test]
fn search_repositories_enums_validate() {
    let cmd = catalog(&["search_repositories"]);
    let value = mapped(
        run(
            &cmd,
            &[
                "search_repositories",
                "--query",
                "q",
                "--sort",
                "stars",
                "--order",
                "desc",
            ],
        )
        .unwrap(),
    );
    assert_eq!(value["sort"], "stars");
    assert_eq!(value["order"], "desc");
    assert!(
        parse(
            &cmd,
            &[
                "search_repositories",
                "--query",
                "q",
                "--sort",
                "stargazers"
            ]
        )
        .is_none()
    );
    assert!(
        parse(
            &cmd,
            &["search_repositories", "--query", "q", "--order", "up"]
        )
        .is_none()
    );
}

#[test]
fn search_repositories_numbers_and_ignored_constraints() {
    let cmd = catalog(&["search_repositories"]);
    // `perPage` rides camelCase verbatim; minimum/maximum are ignored.
    let value = mapped(
        run(
            &cmd,
            &[
                "search_repositories",
                "--query",
                "q",
                "--perPage",
                "1000",
                "--page",
                "2",
            ],
        )
        .unwrap(),
    );
    assert_eq!(value["perPage"].as_f64(), Some(1000.0));
    assert_eq!(value["page"].as_f64(), Some(2.0));
}

#[test]
fn update_pull_request_state_enum_and_number() {
    let cmd = catalog(&["update_pull_request"]);
    let value = mapped(
        run(
            &cmd,
            &[
                "update_pull_request",
                "--owner",
                "o",
                "--repo",
                "r",
                "--pullNumber",
                "7",
                "--state",
                "closed",
            ],
        )
        .unwrap(),
    );
    assert_eq!(value["state"], "closed");
    // `pullNumber` is a schema number: whole values keep the integer
    // wire form strict parsers accept.
    assert_eq!(value["pullNumber"].as_i64(), Some(7));
    assert!(!value["pullNumber"].is_f64());
    // draft is an optional boolean: presence flag.
    assert!(parse(&cmd, &["update_pull_request", "--draft", "false"]).is_none());
}

#[test]
fn create_issue_maps_scalars_and_ignores_extensions() {
    let cmd = catalog(&["create_issue"]);
    let value = mapped(
        run(
            &cmd,
            &[
                "create_issue",
                "--owner",
                "o",
                "--repo",
                "r",
                "--title",
                "t",
                "--body",
                "b",
            ],
        )
        .unwrap(),
    );
    assert_eq!(
        value,
        json!({"owner": "o", "repo": "r", "title": "t", "body": "b"})
    );
    // parent_issue_number (number, minimum ignored) is optional.
    let partial = mapped(
        run(
            &cmd,
            &[
                "create_issue",
                "--owner",
                "o",
                "--repo",
                "r",
                "--title",
                "t",
            ],
        )
        .unwrap(),
    );
    assert_eq!(partial, json!({"owner": "o", "repo": "r", "title": "t"}));
    assert!(parse(&cmd, &["create_issue", "--owner", "o", "--repo", "r"]).is_none());
}

#[test]
fn projects_write_excerpt_maps_both_json_flags() {
    let cmd = catalog(&["projects_write_excerpt"]);
    let value = mapped(
        run(
            &cmd,
            &[
                "projects_write_excerpt",
                "--iterations_json",
                r#"[{"title": "Sprint 1", "start_date": "2026-01-01", "duration": 7}]"#,
                "--updated_field_json",
                r#"{"name": "Status", "value": "In Progress"}"#,
            ],
        )
        .unwrap(),
    );
    // Verbatim: the per-item required shape passes through untouched.
    assert_eq!(
        value,
        json!({
            "iterations": [
                {"title": "Sprint 1", "start_date": "2026-01-01", "duration": 7},
            ],
            "updated_field": {"name": "Status", "value": "In Progress"},
        })
    );
}

#[test]
fn atlassian_additional_fields_shape() {
    let cmd = command(
        "atlassian",
        &[tool(
            "create_issue",
            "create an issue",
            fixture("additional_fields.json"),
        )],
    );
    // The pattern on project_key is ignored — the server validates.
    let value = mapped(
        run(
            &cmd,
            &[
                "create_issue",
                "--project_key",
                "proj",
                "--summary",
                "Fix login",
                "--additional_fields_json",
                r#"{"priority": {"name": "High"}}"#,
            ],
        )
        .unwrap(),
    );
    assert_eq!(
        value,
        json!({
            "project_key": "proj",
            "summary": "Fix login",
            "additional_fields": {"priority": {"name": "High"}},
        })
    );
    // Required scalars are enforced.
    assert!(parse(&cmd, &["create_issue", "--project_key", "PROJ"]).is_none());
    // The three-variant anyOf is a JSON flag, not a nullable string.
    assert!(parse(&cmd, &["create_issue", "--additional_fields", "x"]).is_none());
}

#[test]
fn fastmcp_get_scene_info_shape() {
    let cmd = command(
        "blender",
        &[tool(
            "get_scene_info",
            "get scene info",
            fixture("get_scene_info.json"),
        )],
    );
    // The spec's example: nullable optionals map as plain flags.
    let value = mapped(
        run(
            &cmd,
            &[
                "get_scene_info",
                "--code",
                "import bpy; print(bpy.context.scene.name)",
                "--query",
                "chair",
                "--fields",
                "location",
                "--fields",
                "topology",
                "--limit",
                "5",
            ],
        )
        .unwrap(),
    );
    assert_eq!(
        value,
        json!({
            "code": "import bpy; print(bpy.context.scene.name)",
            "query": "chair",
            "fields": ["location", "topology"],
            "limit": 5,
        })
    );
    // Omitted optionals — user_prompt (default ""), limit (default 20)
    // — stay absent; required code is enforced.
    let minimal = mapped(run(&cmd, &["get_scene_info", "--code", "bpy"]).unwrap());
    assert_eq!(minimal, json!({"code": "bpy"}));
    assert!(parse(&cmd, &["get_scene_info"]).is_none());
}

#[test]
fn whole_catalog_round_trips_through_the_override() {
    let cmd = catalog(&["create_issue", "create_pull_request"]);
    assert_eq!(
        overridden(run(&cmd, &["create_issue", "--json", r#"{"owner": "o"}"#]).unwrap()),
        r#"{"owner": "o"}"#
    );
    // Every tool of the catalog also serves the bare list path: bare
    // invocation parses, args reports NoSubcommand, list_json renders.
    let matches = parse(&cmd, &[]).unwrap();
    assert!(matches!(
        args(&cmd, &matches),
        Err(mcp_to_cli::MappingError::NoSubcommand)
    ));
    let listed = mcp_to_cli::list_json(&[
        tool(
            "create_issue",
            "the create_issue tool",
            fixture("create_issue.json"),
        ),
        tool(
            "create_pull_request",
            "the create_pull_request tool",
            fixture("create_pull_request.json"),
        ),
    ]);
    assert_eq!(listed.as_array().map(Vec::len), Some(2));
}
