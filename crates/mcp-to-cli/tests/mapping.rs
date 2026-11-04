//! The mapping table, row by row, driven through `command` and clap's
//! own argv parsing.

mod common;

use common::{mapped, overridden, parse, run, tool};
use mcp_to_cli::{MappingError, command};
use serde_json::json;

/// An object schema from its properties and `required` list.
fn object_schema(properties: serde_json::Value, required: &[&str]) -> serde_json::Value {
    json!({
        "type": "object",
        "properties": properties,
        "required": required,
    })
}

// --- property names: verbatim, no case translation ---

#[test]
fn property_names_pass_through_verbatim() {
    let cmd = command(
        "srv",
        &[tool(
            "t",
            "d",
            object_schema(json!({"per_page": {"type": "string"}}), &[]),
        )],
    );
    assert_eq!(
        mapped(run(&cmd, &["t", "--per_page", "20"]).unwrap()),
        json!({"per_page": "20"})
    );
    // The camel-cased guess is not a flag.
    assert!(parse(&cmd, &["t", "--perPage", "20"]).is_none());
}

#[test]
fn numbers_map_as_f64() {
    let cmd = command(
        "srv",
        &[tool(
            "t",
            "d",
            object_schema(json!({"ratio": {"type": "number"}}), &[]),
        )],
    );
    let value = mapped(run(&cmd, &["t", "--ratio", "2.5"]).unwrap());
    assert_eq!(value["ratio"].as_f64(), Some(2.5));
    // An integral float stays an f64, not an integer.
    let whole = mapped(run(&cmd, &["t", "--ratio", "3"]).unwrap());
    assert!(whole["ratio"].is_f64());
    assert!(!whole["ratio"].is_i64());
    // clap owns the parse error, the usage path.
    assert!(parse(&cmd, &["t", "--ratio", "two"]).is_none());
}

#[test]
fn integers_map_as_i64() {
    let cmd = command(
        "srv",
        &[tool(
            "t",
            "d",
            object_schema(json!({"limit": {"type": "integer"}}), &[]),
        )],
    );
    let value = mapped(run(&cmd, &["t", "--limit", "5"]).unwrap());
    assert_eq!(value["limit"].as_i64(), Some(5));
    assert!(value["limit"].is_i64());
    assert!(parse(&cmd, &["t", "--limit", "1.5"]).is_none());
    assert!(parse(&cmd, &["t", "--limit", "99999999999999999999"]).is_none());
}

// --- enums: possible_values, validation and completion free ---

#[test]
fn string_enum_earns_possible_values() {
    let cmd = command(
        "srv",
        &[tool(
            "t",
            "d",
            object_schema(
                json!({"state": {"type": "string", "enum": ["open", "closed"]}}),
                &[],
            ),
        )],
    );
    assert_eq!(
        mapped(run(&cmd, &["t", "--state", "closed"]).unwrap()),
        json!({"state": "closed"})
    );
    // clap rejects anything else.
    assert!(parse(&cmd, &["t", "--state", "archived"]).is_none());

    let sc = cmd.find_subcommand("t").expect("tool subcommand");
    let arg = sc
        .get_arguments()
        .find(|a| a.get_long() == Some("state"))
        .expect("state flag");
    let values: Vec<String> = arg
        .get_value_parser()
        .possible_values()
        .expect("enum parser lists values")
        .map(|v| v.get_name().to_owned())
        .collect();
    assert_eq!(values, ["open", "closed"]);
}

#[test]
fn enum_without_type_is_still_a_string_enum() {
    let cmd = command(
        "srv",
        &[tool(
            "t",
            "d",
            object_schema(json!({"kind": {"enum": ["a", "b"]}}), &[]),
        )],
    );
    assert_eq!(
        mapped(run(&cmd, &["t", "--kind", "a"]).unwrap()),
        json!({"kind": "a"})
    );
}

#[test]
fn non_string_enum_falls_back_to_the_json_flag() {
    let cmd = command(
        "srv",
        &[tool(
            "t",
            "d",
            object_schema(json!({"num": {"type": "integer", "enum": [1, 2, 3]}}), &[]),
        )],
    );
    let sc = cmd.find_subcommand("t").expect("tool subcommand");
    let longs: Vec<&str> = sc.get_arguments().filter_map(|a| a.get_long()).collect();
    assert!(!longs.contains(&"num"));
    assert!(longs.contains(&"num_json"));
    assert_eq!(
        mapped(run(&cmd, &["t", "--num_json", "2"]).unwrap()),
        json!({"num": 2})
    );
    assert!(parse(&cmd, &["t", "--num", "2"]).is_none());
}

// --- booleans: never guessed ---

#[test]
fn required_boolean_is_a_true_false_value_flag() {
    let cmd = command(
        "srv",
        &[tool(
            "t",
            "d",
            object_schema(json!({"force": {"type": "boolean"}}), &["force"]),
        )],
    );
    assert_eq!(
        mapped(run(&cmd, &["t", "--force", "false"]).unwrap()),
        json!({"force": false})
    );
    assert_eq!(
        mapped(run(&cmd, &["t", "--force", "true"]).unwrap()),
        json!({"force": true})
    );
    // Required: bare tool and valueless flag are both clap errors.
    assert!(parse(&cmd, &["t"]).is_none());
    assert!(parse(&cmd, &["t", "--force"]).is_none());
    // Exactly true/false.
    assert!(parse(&cmd, &["t", "--force", "maybe"]).is_none());
    assert!(parse(&cmd, &["t", "--force", "1"]).is_none());
}

#[test]
fn optional_boolean_is_a_presence_flag() {
    let cmd = command(
        "srv",
        &[tool(
            "t",
            "d",
            object_schema(json!({"draft": {"type": "boolean"}}), &[]),
        )],
    );
    // Absent means the key stays absent.
    assert_eq!(mapped(run(&cmd, &["t"]).unwrap()), json!({}));
    assert_eq!(
        mapped(run(&cmd, &["t", "--draft"]).unwrap()),
        json!({"draft": true})
    );
    // A presence flag takes no value.
    assert!(parse(&cmd, &["t", "--draft", "false"]).is_none());
}

#[test]
fn optional_boolean_default_false_is_still_a_presence_flag() {
    let cmd = command(
        "srv",
        &[tool(
            "t",
            "d",
            object_schema(
                json!({"verbose": {"type": "boolean", "default": false}}),
                &[],
            ),
        )],
    );
    assert_eq!(mapped(run(&cmd, &["t"]).unwrap()), json!({}));
    assert_eq!(
        mapped(run(&cmd, &["t", "--verbose"]).unwrap()),
        json!({"verbose": true})
    );
}

#[test]
fn optional_boolean_default_true_stays_an_explicit_value_flag() {
    let cmd = command(
        "srv",
        &[tool(
            "t",
            "d",
            object_schema(
                json!({"minimal_output": {"type": "boolean", "default": true}}),
                &[],
            ),
        )],
    );
    // Omitted: the key stays absent, the server applies its default.
    assert_eq!(mapped(run(&cmd, &["t"]).unwrap()), json!({}));
    assert_eq!(
        mapped(run(&cmd, &["t", "--minimal_output", "false"]).unwrap()),
        json!({"minimal_output": false})
    );
    assert_eq!(
        mapped(run(&cmd, &["t", "--minimal_output", "true"]).unwrap()),
        json!({"minimal_output": true})
    );
    // And it is not a presence flag: the value is mandatory.
    assert!(parse(&cmd, &["t", "--minimal_output"]).is_none());
}

#[test]
fn nullable_boolean_keeps_the_value_flag() {
    let cmd = command(
        "srv",
        &[tool(
            "t",
            "d",
            object_schema(
                json!({"flag": {"anyOf": [{"type": "boolean"}, {"type": "null"}]}}),
                &[],
            ),
        )],
    );
    // A presence flag could not say false; nullability only forces
    // optionality.
    assert_eq!(
        mapped(run(&cmd, &["t", "--flag", "false"]).unwrap()),
        json!({"flag": false})
    );
    assert_eq!(mapped(run(&cmd, &["t"]).unwrap()), json!({}));
}

// --- defaults are never pre-filled ---

#[test]
fn schema_defaults_are_never_prefilled() {
    let cmd = command(
        "srv",
        &[tool(
            "t",
            "d",
            object_schema(
                json!({
                    "limit": {"type": "integer", "default": 20},
                    "name": {"type": "string", "default": "x"},
                }),
                &[],
            ),
        )],
    );
    assert_eq!(mapped(run(&cmd, &["t"]).unwrap()), json!({}));
}

// --- arrays ---

#[test]
fn array_of_strings_is_repeatable() {
    let cmd = command(
        "srv",
        &[tool(
            "t",
            "d",
            object_schema(
                json!({"tags": {"type": "array", "items": {"type": "string"}}}),
                &[],
            ),
        )],
    );
    assert_eq!(
        mapped(run(&cmd, &["t", "--tags", "a", "--tags", "b"]).unwrap()),
        json!({"tags": ["a", "b"]})
    );
    // One occurrence is a one-element array, not a bare string.
    assert_eq!(
        mapped(run(&cmd, &["t", "--tags", "a"]).unwrap()),
        json!({"tags": ["a"]})
    );
    assert_eq!(mapped(run(&cmd, &["t"]).unwrap()), json!({}));
}

#[test]
fn arrays_of_numbers_integers_and_booleans_repeat() {
    let cmd = command(
        "srv",
        &[tool(
            "t",
            "d",
            object_schema(
                json!({
                    "weights": {"type": "array", "items": {"type": "number"}},
                    "counts": {"type": "array", "items": {"type": "integer"}},
                    "flags": {"type": "array", "items": {"type": "boolean"}},
                }),
                &[],
            ),
        )],
    );
    assert_eq!(
        mapped(run(&cmd, &["t", "--weights", "1.5", "--weights", "2"]).unwrap()),
        json!({"weights": [1.5, 2.0]})
    );
    assert_eq!(
        mapped(run(&cmd, &["t", "--counts", "7", "--counts", "9"]).unwrap()),
        json!({"counts": [7, 9]})
    );
    assert_eq!(
        mapped(run(&cmd, &["t", "--flags", "true", "--flags", "false"]).unwrap()),
        json!({"flags": [true, false]})
    );
    assert!(parse(&cmd, &["t", "--counts", "1.5"]).is_none());
}

#[test]
fn array_of_enum_strings_repeats_with_possible_values() {
    let cmd = command(
        "srv",
        &[tool(
            "t",
            "d",
            object_schema(
                json!({"levels": {
                    "type": "array",
                    "items": {"type": "string", "enum": ["low", "high"]},
                }}),
                &[],
            ),
        )],
    );
    assert_eq!(
        mapped(run(&cmd, &["t", "--levels", "low", "--levels", "high"]).unwrap()),
        json!({"levels": ["low", "high"]})
    );
    assert!(parse(&cmd, &["t", "--levels", "mid"]).is_none());
}

#[test]
fn array_of_nullable_strings_is_repeatable() {
    let cmd = command(
        "srv",
        &[tool(
            "t",
            "d",
            object_schema(
                json!({"fields": {
                    "type": "array",
                    "items": {"anyOf": [{"type": "string"}, {"type": "null"}]},
                }}),
                &[],
            ),
        )],
    );
    assert_eq!(
        mapped(run(&cmd, &["t", "--fields", "location", "--fields", "topology"]).unwrap()),
        json!({"fields": ["location", "topology"]})
    );
}

// --- the JSON escape hatch ---

#[test]
fn objects_take_the_json_flag_verbatim() {
    let cmd = command(
        "srv",
        &[tool(
            "t",
            "d",
            object_schema(json!({"cfg": {"type": "object"}}), &[]),
        )],
    );
    let raw = r#"{"b": 1, "a": [true, null]}"#;
    assert_eq!(
        mapped(run(&cmd, &["t", "--cfg_json", raw]).unwrap()),
        json!({"cfg": {"b": 1, "a": [true, null]}})
    );
    assert!(parse(&cmd, &["t", "--cfg_json", "not json"]).is_none());
    assert!(parse(&cmd, &["t", "--cfg", "{}"]).is_none());
}

#[test]
fn anyof_of_three_variants_takes_the_json_flag() {
    let cmd = command(
        "srv",
        &[tool(
            "t",
            "d",
            object_schema(
                json!({"additional_fields": {
                    "anyOf": [{"type": "object"}, {"type": "string"}, {"type": "null"}],
                }}),
                &[],
            ),
        )],
    );
    assert_eq!(
        mapped(run(&cmd, &["t", "--additional_fields_json", r#""notes""#]).unwrap()),
        json!({"additional_fields": "notes"})
    );
    assert_eq!(
        mapped(
            run(
                &cmd,
                &[
                    "t",
                    "--additional_fields_json",
                    r#"{"priority": {"name": "High"}}"#
                ]
            )
            .unwrap()
        ),
        json!({"additional_fields": {"priority": {"name": "High"}}})
    );
}

#[test]
fn oneof_and_allof_take_the_json_flag() {
    let schema = json!({
        "updated_field": {
            "oneOf": [
                {"type": "object", "required": ["id"]},
                {"type": "object", "required": ["name"]},
            ],
        },
        "merged": {"allOf": [{"type": "string"}]},
    });
    let cmd = command("srv", &[tool("t", "d", object_schema(schema, &[]))]);
    assert_eq!(
        mapped(
            run(
                &cmd,
                &["t", "--updated_field_json", r#"{"name": "Status"}"#]
            )
            .unwrap()
        ),
        json!({"updated_field": {"name": "Status"}})
    );
    let longs: Vec<&str> = cmd
        .find_subcommand("t")
        .unwrap()
        .get_arguments()
        .filter_map(|a| a.get_long())
        .collect();
    assert!(longs.contains(&"merged_json"));
}

#[test]
fn unknown_and_missing_types_take_the_json_flag() {
    let cmd = command(
        "srv",
        &[tool(
            "t",
            "d",
            object_schema(json!({"weird": {"type": "binary"}, "anything": {}}), &[]),
        )],
    );
    assert_eq!(
        mapped(run(&cmd, &["t", "--weird_json", r#"[1, 2]"#]).unwrap()),
        json!({"weird": [1, 2]})
    );
    assert_eq!(
        mapped(run(&cmd, &["t", "--anything_json", "null"]).unwrap()),
        json!({"anything": null})
    );
}

#[test]
fn arrays_of_objects_and_nested_arrays_take_the_json_flag() {
    let cmd = command(
        "srv",
        &[tool(
            "t",
            "d",
            object_schema(
                json!({
                    "items": {"type": "array", "items": {"type": "object"}},
                    "matrix": {"type": "array", "items": {"type": "array", "items": {"type": "string"}}},
                    "untyped": {"type": "array"},
                }),
                &[],
            ),
        )],
    );
    assert_eq!(
        mapped(run(&cmd, &["t", "--items_json", r#"[{"a": 1}]"#]).unwrap()),
        json!({"items": [{"a": 1}]})
    );
    assert_eq!(
        mapped(run(&cmd, &["t", "--matrix_json", r#"[["a"]]"#]).unwrap()),
        json!({"matrix": [["a"]]})
    );
    assert_eq!(
        mapped(run(&cmd, &["t", "--untyped_json", "3"]).unwrap()),
        json!({"untyped": 3})
    );
    // The repeatable form does not exist for these.
    assert!(parse(&cmd, &["t", "--items", "x"]).is_none());
}

// --- nullability: the FastMCP Optional[T] shape ---

#[test]
fn nullable_string_maps_as_a_plain_optional_string() {
    let cmd = command(
        "srv",
        &[tool(
            "t",
            "d",
            object_schema(
                json!({"query": {"anyOf": [{"type": "string"}, {"type": "null"}]}}),
                &[],
            ),
        )],
    );
    assert_eq!(
        mapped(run(&cmd, &["t", "--query", "chair"]).unwrap()),
        json!({"query": "chair"})
    );
    assert_eq!(mapped(run(&cmd, &["t"]).unwrap()), json!({}));
}

#[test]
fn nullable_type_arrays_unwrap() {
    let cmd = command(
        "srv",
        &[tool(
            "t",
            "d",
            object_schema(json!({"q": {"type": ["string", "null"]}}), &[]),
        )],
    );
    assert_eq!(
        mapped(run(&cmd, &["t", "--q", "x"]).unwrap()),
        json!({"q": "x"})
    );
    let longs: Vec<&str> = cmd
        .find_subcommand("t")
        .unwrap()
        .get_arguments()
        .filter_map(|a| a.get_long())
        .collect();
    assert!(longs.contains(&"q"));
    assert!(!longs.contains(&"q_json"));
}

#[test]
fn nullable_integer_with_default_omits_when_absent() {
    let cmd = command(
        "srv",
        &[tool(
            "t",
            "d",
            object_schema(
                json!({"limit": {"anyOf": [{"type": "integer"}, {"type": "null"}], "default": 20}}),
                &[],
            ),
        )],
    );
    assert_eq!(
        mapped(run(&cmd, &["t", "--limit", "5"]).unwrap()),
        json!({"limit": 5})
    );
    // Not 20: the default is the server's business.
    assert_eq!(mapped(run(&cmd, &["t"]).unwrap()), json!({}));
}

#[test]
fn nullable_properties_are_never_clap_required() {
    let cmd = command(
        "srv",
        &[tool(
            "t",
            "d",
            object_schema(
                json!({"q": {"anyOf": [{"type": "string"}, {"type": "null"}]}}),
                &["q"],
            ),
        )],
    );
    // `required` lists it, but null is a value the flag cannot say, so
    // clap lets the tool run bare.
    assert_eq!(mapped(run(&cmd, &["t"]).unwrap()), json!({}));
}

// --- required ---

#[test]
fn required_scalars_are_clap_required() {
    let cmd = command(
        "srv",
        &[tool(
            "t",
            "d",
            object_schema(
                json!({"owner": {"type": "string"}, "n": {"type": "number"}, "i": {"type": "integer"}}),
                &["owner", "n", "i"],
            ),
        )],
    );
    assert!(parse(&cmd, &["t"]).is_none());
    assert!(parse(&cmd, &["t", "--owner", "o", "--n", "1.5"]).is_none());
    assert!(parse(&cmd, &["t", "--owner", "o", "--n", "1.5", "--i", "2"]).is_some());
}

#[test]
fn required_json_properties_are_never_clap_required() {
    let cmd = command(
        "srv",
        &[tool(
            "t",
            "d",
            object_schema(json!({"cfg": {"type": "object"}}), &["cfg"]),
        )],
    );
    // Server-side validation owns these.
    assert_eq!(mapped(run(&cmd, &["t"]).unwrap()), json!({}));
    assert_eq!(
        mapped(run(&cmd, &["t", "--cfg_json", r#"{"a": 1}"#]).unwrap()),
        json!({"cfg": {"a": 1}})
    );
}

// --- ignored keywords and extensions: the server stays the validator ---

#[test]
fn constraint_keywords_are_ignored() {
    let cmd = command(
        "srv",
        &[tool(
            "t",
            "d",
            object_schema(
                json!({
                    "key": {"type": "string", "pattern": "[A-Z]+", "format": "uri", "minLength": 3},
                    "n": {"type": "number", "minimum": 1, "maximum": 100},
                }),
                &[],
            ),
        )],
    );
    assert_eq!(
        mapped(run(&cmd, &["t", "--key", "lower", "--n", "5000"]).unwrap()),
        json!({"key": "lower", "n": 5000.0})
    );
}

#[test]
fn unknown_extension_keys_are_ignored() {
    let cmd = command(
        "srv",
        &[tool(
            "t",
            "d",
            object_schema(
                json!({"owner": {"type": "string", "x-mcp-header": "owner"}}),
                &["owner"],
            ),
        )],
    );
    assert_eq!(
        mapped(run(&cmd, &["t", "--owner", "acme"]).unwrap()),
        json!({"owner": "acme"})
    );
}

// --- the --json payload override ---

#[test]
fn json_override_wins_verbatim() {
    let cmd = command(
        "srv",
        &[tool(
            "t",
            "d",
            object_schema(json!({"owner": {"type": "string"}}), &[]),
        )],
    );
    let raw = r#"{ "owner" : "acme", "extra" : [1, 2] }"#;
    assert_eq!(overridden(run(&cmd, &["t", "--json", raw]).unwrap()), raw);
    // The stdin marker rides through untouched; reading it is the
    // caller's job.
    assert_eq!(overridden(run(&cmd, &["t", "--json", "-"]).unwrap()), "-");
}

#[test]
fn json_override_relaxes_required_flags() {
    let cmd = command(
        "srv",
        &[tool(
            "t",
            "d",
            object_schema(json!({"owner": {"type": "string"}}), &["owner"]),
        )],
    );
    // The override replaces the payload, so the required flag does
    // not apply.
    assert_eq!(
        overridden(run(&cmd, &["t", "--json", r#"{"owner": "o"}"#]).unwrap()),
        r#"{"owner": "o"}"#
    );
    // But it still cannot ride alongside the flag it replaces.
    assert_eq!(
        run(&cmd, &["t", "--json", "{}", "--owner", "o"]).unwrap_err(),
        MappingError::JsonConflict
    );
}

#[test]
fn json_override_conflicts_with_tool_flags() {
    let cmd = command(
        "srv",
        &[tool(
            "t",
            "d",
            object_schema(json!({"owner": {"type": "string"}}), &[]),
        )],
    );
    assert_eq!(
        run(&cmd, &["t", "--json", "{}", "--owner", "acme"]).unwrap_err(),
        MappingError::JsonConflict
    );
    // A presence flag counts as a tool flag too.
    let bools = command(
        "srv",
        &[tool(
            "t",
            "d",
            object_schema(json!({"draft": {"type": "boolean"}}), &[]),
        )],
    );
    assert_eq!(
        run(&bools, &["t", "--json", "{}", "--draft"]).unwrap_err(),
        MappingError::JsonConflict
    );
}

#[test]
fn json_override_serves_a_demoted_tool() {
    let cmd = command(
        "srv",
        &[tool(
            "t",
            "d",
            object_schema(
                json!({"json": {"type": "string"}, "other": {"type": "string"}}),
                &[],
            ),
        )],
    );
    let longs: Vec<&str> = cmd
        .find_subcommand("t")
        .unwrap()
        .get_arguments()
        .filter_map(|a| a.get_long())
        .collect();
    // The property named `json` collides with the override: the whole
    // tool is demoted to `--json` only.
    assert_eq!(longs, ["json"]);
    assert!(parse(&cmd, &["t", "--other", "x"]).is_none());
    assert_eq!(mapped(run(&cmd, &["t"]).unwrap()), json!({}));
    assert_eq!(
        overridden(run(&cmd, &["t", "--json", r#"{"json": "x"}"#]).unwrap()),
        r#"{"json": "x"}"#
    );
}

// --- collisions demote the whole tool ---

#[test]
fn derived_flag_collision_demotes_the_tool() {
    // `a` (object) derives `--a_json`; a sibling literally named
    // `a_json` would collide with it.
    let cmd = command(
        "srv",
        &[tool(
            "t",
            "d",
            object_schema(
                json!({"a": {"type": "object"}, "a_json": {"type": "string"}}),
                &[],
            ),
        )],
    );
    let longs: Vec<&str> = cmd
        .find_subcommand("t")
        .unwrap()
        .get_arguments()
        .filter_map(|a| a.get_long())
        .collect();
    assert_eq!(longs, ["json"]);
    assert!(parse(&cmd, &["t", "--a_json", "x"]).is_none());
    assert!(parse(&cmd, &["t", "--a", "x"]).is_none());
    assert_eq!(
        overridden(run(&cmd, &["t", "--json", r#"{"a": {}}"#]).unwrap()),
        r#"{"a": {}}"#
    );
}

#[test]
fn help_named_property_demotes_the_tool() {
    let cmd = command(
        "srv",
        &[tool(
            "t",
            "d",
            object_schema(json!({"help": {"type": "string"}}), &[]),
        )],
    );
    // `--help` is clap's; the tool runs bare and takes only `--json`.
    assert_eq!(mapped(run(&cmd, &["t"]).unwrap()), json!({}));
    assert_eq!(overridden(run(&cmd, &["t", "--json", "{}"]).unwrap()), "{}");
}

#[test]
fn unusable_property_name_demotes_the_tool() {
    let cmd = command(
        "srv",
        &[tool(
            "t",
            "d",
            object_schema(
                json!({"bad name": {"type": "string"}, "ok": {"type": "string"}}),
                &[],
            ),
        )],
    );
    // Whitespace cannot ride a long flag; the conservative cut drops
    // every property flag rather than half a tool.
    let longs: Vec<&str> = cmd
        .find_subcommand("t")
        .unwrap()
        .get_arguments()
        .filter_map(|a| a.get_long())
        .collect();
    assert_eq!(longs, ["json"]);
    assert!(parse(&cmd, &["t", "--ok", "x"]).is_none());
}

// --- degenerate schemas ---

#[test]
fn empty_schema_tool_maps_an_empty_object() {
    let cmd = command("srv", &[tool("t", "d", json!({}))]);
    assert!(cmd.find_subcommand("t").is_some());
    assert_eq!(mapped(run(&cmd, &["t"]).unwrap()), json!({}));
}

#[test]
fn object_schema_without_properties_maps_an_empty_object() {
    let cmd = command(
        "srv",
        &[tool("t", "d", json!({"type": "object", "properties": {}}))],
    );
    assert_eq!(mapped(run(&cmd, &["t"]).unwrap()), json!({}));
    assert_eq!(
        overridden(run(&cmd, &["t", "--json", r#"{"a": 1}"#]).unwrap()),
        r#"{"a": 1}"#
    );
}
