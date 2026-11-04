//! Map MCP tool catalogs to clap commands, and argv back to JSON.
//!
//! An MCP server advertises its tools as `(name, description, JSON
//! Schema)` triples; this crate turns each CLI-safe tool into a clap
//! subcommand whose flags come from the schema's top-level properties,
//! then turns parsed argv back into the `{property: value}` object the
//! server expects. It is pure mapping: no I/O, no processes, no MCP
//! protocol, no rhema dependencies — `clap`, `serde_json`, and
//! `thiserror` are the whole list.
//!
//! The mapping is conservative, with escape hatches at every level of
//! schema complexity: enums become clap `possible_values`, scalar
//! arrays become repeatable flags, and every shape the flag form
//! cannot express collapses to a `--<property>_json` flag or to the
//! per-tool `--json` payload override. Schema defaults are never
//! pre-filled — an omitted flag omits the key — and the server stays
//! the validator: constraint keywords are ignored and JSON-flag
//! properties are never clap-required.

#![deny(missing_docs)]
#![forbid(unsafe_code)]

use std::collections::BTreeSet;

use clap::builder::PossibleValuesParser;
use clap::{Arg, ArgAction, ArgMatches, Command};
use serde_json::{Map, Number, Value};

/// One advertised MCP tool: its name, description, and input schema.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolInfo {
    /// The tool's name as the server advertises it, e.g. `create_issue`.
    pub name: String,
    /// The tool's description, flowed into the subcommand's help verbatim.
    pub description: String,
    /// The tool's JSON Schema input: the wire format's `inputSchema`
    /// object, read as-is.
    pub input_schema: Value,
}

/// The arguments for one tool call, recovered from parsed argv.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum Payload {
    /// The mapped `{property: value}` object: one entry per flag the
    /// caller passed, absent flags leaving absent keys.
    Mapped(Value),
    /// The raw `--json` string, verbatim. `-` means stdin and is the
    /// caller's to read; this crate never touches I/O.
    Overridden(String),
}

/// Why parsed argv could not be mapped to tool arguments.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
#[non_exhaustive]
pub enum MappingError {
    /// No subcommand matched: the invocation was bare, and the tool
    /// list ([`list_json`]) is the answer.
    #[error("no subcommand was matched; a bare invocation lists the tools")]
    NoSubcommand,
    /// The matches carry a subcommand the command does not define.
    #[error("unknown subcommand `{name}`")]
    UnknownSubcommand {
        /// The subcommand name found in the matches but missing from
        /// the command.
        name: String,
    },
    /// The `--json` payload override was combined with the tool's own
    /// flags.
    #[error("the --json override cannot be combined with the tool's own flags")]
    JsonConflict,
}

/// Build the root command for one server's catalog: one subcommand per
/// CLI-safe tool, flags from the tool's input schema.
///
/// The returned [`Command`] requires no subcommand — a bare invocation
/// is the caller's cue to print [`list_json`] — and carries no
/// decoration of its own, so the caller is free to add `.about()`,
/// `.version()`, and friends. A tool becomes a subcommand when
/// [`cli_safe`] accepts its name, its name is not `help` (clap owns
/// that subcommand), and its schema's root is an object; an explicit
/// non-object `"type"` omits the tool, a missing `"type"` is read as
/// the object shape every tool call has. Tools left out are reachable
/// only through the caller's raw-call path; duplicate names keep the
/// first occurrence.
///
/// Each tool renders per its schema, property names passing through
/// verbatim (`--per_page` — no case translation, no aliases):
///
/// - `string` is a value flag; a string `enum` adds clap
///   `possible_values`, so validation and completion come free. An
///   enum with any non-string value falls back to the JSON flag.
/// - `number` parses as `f64`, `integer` as `i64`.
/// - A required boolean is a value flag accepting exactly `true` or
///   `false`. An optional boolean with no `default` (or `default:
///   false`) is a presence flag — passing it maps `true`, omitting it
///   omits the key. With `default: true` it stays a value flag, never
///   a guess.
/// - An array of scalars is a repeatable flag (`--tag a --tag b`,
///   one value per occurrence); an array of anything else takes the
///   JSON flag.
/// - Objects, `anyOf`/`oneOf`/`allOf`, and unknown or missing types
///   map to `--<property>_json`, taking one raw JSON value verbatim.
///   A two-variant `anyOf`/`oneOf` of `{"type": "null"}` and one other
///   shape — the `Optional[T]` form FastMCP emits for every optional
///   parameter — unwraps to that other shape first, recursively and
///   through array `items`; so does a `type` array of `"null"` plus
///   one other type. Nullable properties stay optional: JSON `null` is
///   a value the flag form cannot say, so they are never
///   clap-required.
/// - A property in the schema's `required` array is clap-required —
///   but only when its flag can express the value, and never beneath
///   the `--json` override, which replaces the whole payload.
///   JSON-flag properties are never clap-required and stay the
///   server's to validate.
/// - Schema `default`s are never pre-filled. Constraint keywords
///   (`pattern`, `minimum`, `maximum`, `format`, ...) and unknown
///   extension keys (`x-mcp-header`) are ignored.
///
/// Two collisions demote a tool to a subcommand with no property
/// flags, reachable only through `--json`: a property literally named
/// `json` would collide with the payload override, and any flag
/// derived from the tool that collides with another (`help` or `json`
/// as a property name, or a `<property>_json` flag against a sibling)
/// does the same. Every subcommand also carries the `--json <JSON>`
/// payload override; see [`args`].
#[must_use]
pub fn command(server_name: &str, catalog: &[ToolInfo]) -> Command {
    let mut root = Command::new(server_name.to_owned()).subcommand_required(false);
    let mut seen = BTreeSet::new();
    for tool in catalog {
        if !renderable(tool) || !seen.insert(tool.name.as_str()) {
            continue;
        }
        root = root.subcommand(tool_subcommand(tool));
    }
    root
}

/// Recover one tool call's arguments from matches parsed against
/// [`command`]'s tree.
///
/// The matched subcommand's flags become the [`Payload::Mapped`]
/// object, each passed flag contributing its key and absent flags
/// leaving absent keys — schema defaults included, never pre-filled.
/// A lone `--json` wins as [`Payload::Overridden`], carrying the raw
/// string untouched; `-` means stdin and is never read here. `--json`
/// next to any of the tool's own flags is [`MappingError::JsonConflict`].
///
/// # Errors
///
/// - [`MappingError::NoSubcommand`] when `matches` has no subcommand:
///   a bare invocation, answered with [`list_json`].
/// - [`MappingError::UnknownSubcommand`] when the matched subcommand
///   is not part of `cmd`.
/// - [`MappingError::JsonConflict`] when `--json` rides alongside the
///   tool's own flags.
pub fn args(cmd: &Command, matches: &ArgMatches) -> Result<Payload, MappingError> {
    let (name, sub) = matches.subcommand().ok_or(MappingError::NoSubcommand)?;
    let sc = cmd
        .find_subcommand(name)
        .ok_or_else(|| MappingError::UnknownSubcommand {
            name: name.to_owned(),
        })?;

    let mut object = Map::new();
    for arg in sc.get_arguments() {
        let id = arg.get_id().as_str();
        if id == "json" || id == "help" || id == "version" {
            continue;
        }
        let Some(long) = arg.get_long() else {
            continue;
        };
        let Some(slot) = slot_of_id(id) else {
            continue;
        };
        insert_flag(&mut object, sub, id, long, slot, arg.get_action());
    }

    if let Some(raw) = sub.get_one::<String>("json") {
        if !object.is_empty() {
            return Err(MappingError::JsonConflict);
        }
        return Ok(Payload::Overridden(raw.clone()));
    }
    Ok(Payload::Mapped(Value::Object(object)))
}

/// Whether a tool name can ride argv: non-empty `[a-zA-Z0-9_-]` only.
///
/// The charset test only — a name passing it can still be skipped by
/// [`command`] when clap itself owns it (`help`).
#[must_use]
pub fn cli_safe(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// Render the bare-invocation tool list: `[{name, description}]`.
///
/// The list mirrors [`command`]'s tree exactly — the same tools, in
/// catalog order, duplicates collapsed first-wins — so what it
/// advertises is exactly what the subcommands can run.
#[must_use]
pub fn list_json(catalog: &[ToolInfo]) -> Value {
    let mut seen = BTreeSet::new();
    let tools = catalog
        .iter()
        .filter(|tool| renderable(tool) && seen.insert(tool.name.as_str()))
        .map(|tool| {
            [
                ("name".to_owned(), Value::String(tool.name.clone())),
                (
                    "description".to_owned(),
                    Value::String(tool.description.clone()),
                ),
            ]
            .into_iter()
            .collect::<Map<String, Value>>()
        })
        .collect::<Vec<Map<String, Value>>>();
    Value::Array(tools.into_iter().map(Value::Object).collect())
}

/// The clap-side value kind of one property.
///
/// The kind is tagged into the arg's id (a private encoding: `t`, `e`,
/// `n`, `i`, `b`, `j`, or `f` for the presence flag) so [`args`] can
/// extract typed values from a rebuilt [`Command`], which carries no
/// schema.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Kind {
    /// Free-form string.
    Text,
    /// String restricted by the schema's `enum`.
    Enum,
    /// JSON number, parsed as `f64`.
    Number,
    /// JSON integer, parsed as `i64`.
    Integer,
    /// Boolean value, exactly `true` or `false`.
    Bool,
    /// One raw JSON value, verbatim.
    Json,
}

impl Kind {
    /// The id tag for one value of this kind.
    const fn tag(self) -> char {
        match self {
            Kind::Text => 't',
            Kind::Enum => 'e',
            Kind::Number => 'n',
            Kind::Integer => 'i',
            Kind::Bool => 'b',
            Kind::Json => 'j',
        }
    }

    /// The help placeholder for one value; text and enums show the
    /// property's own name, the rest name their shape.
    fn value_name(self, prop: &str) -> String {
        match self {
            Kind::Text | Kind::Enum => prop.to_uppercase(),
            Kind::Number => "NUMBER".to_owned(),
            Kind::Integer => "INTEGER".to_owned(),
            Kind::Bool => "BOOL".to_owned(),
            Kind::Json => "JSON".to_owned(),
        }
    }
}

/// The flag shape one property maps to.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Shape {
    /// One value per flag: `--field value`.
    Value(Kind),
    /// Repeatable, one value per occurrence: an array of scalars.
    Repeat(Kind),
    /// Presence flag: an optional boolean, `true` when passed.
    SetTrue,
    /// The JSON escape: `--<property>_json`, one raw JSON value.
    Json,
}

/// The id slot one built arg decodes back to in [`args`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Slot {
    /// A presence flag; only `true` is ever stored.
    Presence,
    /// A value or repeatable flag of the given kind.
    Valued(Kind),
}

fn slot_of_id(id: &str) -> Option<Slot> {
    match id.chars().next()? {
        'f' => Some(Slot::Presence),
        't' => Some(Slot::Valued(Kind::Text)),
        'e' => Some(Slot::Valued(Kind::Enum)),
        'n' => Some(Slot::Valued(Kind::Number)),
        'i' => Some(Slot::Valued(Kind::Integer)),
        'b' => Some(Slot::Valued(Kind::Bool)),
        'j' => Some(Slot::Valued(Kind::Json)),
        _ => None,
    }
}

/// Whether a tool joins the clap tree.
fn renderable(tool: &ToolInfo) -> bool {
    tool.name != "help" && cli_safe(&tool.name) && root_is_object(&tool.input_schema)
}

/// Whether a schema's root is object-shaped: a JSON object carrying an
/// explicit `"object"` type, or none at all — `{}` included, since the
/// missing `"type"` is the common tool-schema shape.
fn root_is_object(schema: &Value) -> bool {
    if !schema.is_object() {
        return false;
    }
    match schema.get("type") {
        None => true,
        Some(t) => t.as_str() == Some("object"),
    }
}

fn tool_subcommand(tool: &ToolInfo) -> Command {
    let mut cmd = Command::new(tool.name.clone()).about(&tool.description);
    let mut used = vec!["help".to_owned(), "json".to_owned()];
    let mut demoted = false;

    if let Some(props) = properties(&tool.input_schema) {
        let required = required_names(&tool.input_schema);
        for (index, (prop, schema)) in props.iter().enumerate() {
            let Some(arg) = property_arg(
                prop,
                schema,
                index,
                required.contains(&prop.as_str()),
                &mut used,
            ) else {
                demoted = true;
                break;
            };
            cmd = cmd.arg(arg);
        }
    }
    if demoted {
        // Collision between derived flags: no property flags at all,
        // the tool stays reachable only through `--json`.
        cmd = Command::new(tool.name.clone()).about(&tool.description);
    }
    cmd.arg(override_arg())
}

/// The per-tool `--json` whole-payload override, present on every
/// subcommand.
fn override_arg() -> Arg {
    Arg::new("json")
        .long("json")
        .value_name("JSON")
        .help("Replace the whole payload with this JSON ('-' means stdin, read by the caller)")
}

/// The flag for one property, or `None` when the property would make
/// derived flags collide — which demotes the whole tool.
fn property_arg(
    prop: &str,
    schema: &Value,
    index: usize,
    prop_required: bool,
    used: &mut Vec<String>,
) -> Option<Arg> {
    let mut shape = classify(schema);
    if shape == Shape::Value(Kind::Bool)
        && !prop_required
        && !is_nullable(schema)
        && !default_true(schema)
    {
        // Optional boolean, no default to honor: presence means true,
        // absence means the key stays absent. Required booleans and
        // nullable ones keep the explicit `true|false` value flag.
        shape = Shape::SetTrue;
    }
    let long = match shape {
        Shape::Json => format!("{prop}_json"),
        Shape::Value(_) | Shape::Repeat(_) | Shape::SetTrue => prop.to_owned(),
    };
    if !flag_name_ok(&long) || used.contains(&long) {
        return None;
    }
    used.push(long.clone());

    let help = schema.get("description").and_then(Value::as_str);
    // clap-required only when the flags are the whole answer: JSON-flag
    // properties and nullable ones stay optional. clap cannot spell
    // `required_unless_present` args out in its usage line, so the
    // required mark rides the help text instead.
    let enforced = prop_required
        && !is_nullable(schema)
        && matches!(shape, Shape::Value(_) | Shape::Repeat(_));
    let mut arg = Arg::new(arg_id(shape, index)).long(long);
    match shape {
        Shape::SetTrue => arg = arg.action(ArgAction::SetTrue),
        Shape::Json => {
            arg = arg
                .value_name("JSON")
                .value_parser(clap::builder::ValueParser::new(json_value));
        }
        Shape::Value(kind) => {
            arg = typed_arg(arg, kind, schema, prop);
            // A JSON-flag property can never be clap-required; a
            // nullable one neither, since null is a value the flag
            // cannot say.
            if prop_required && !is_nullable(schema) {
                // Required unless the override carries the payload —
                // the one case the flags are not the answer.
                arg = arg.required_unless_present("json");
            }
        }
        Shape::Repeat(kind) => {
            // The enum values, where any, live on the items schema.
            let items = schema.get("items").unwrap_or(schema);
            arg = typed_arg(arg.action(ArgAction::Append), kind, items, prop);
            if enforced {
                arg = arg.required_unless_present("json");
            }
            arg = arg.help(flag_help(help, enforced, true));
        }
    }
    if !matches!(shape, Shape::Repeat(_)) {
        arg = with_help(arg, help, enforced);
    }
    Some(arg)
}

/// One raw JSON value; clap's parse step owns the rejection of bad
/// JSON, the usage-error path.
fn json_value(raw: &str) -> Result<Value, serde_json::Error> {
    serde_json::from_str(raw)
}

/// The value name and parser for one scalar value flag.
fn typed_arg(arg: Arg, kind: Kind, schema: &Value, prop: &str) -> Arg {
    let arg = arg.value_name(kind.value_name(prop));
    match kind {
        Kind::Text => arg,
        Kind::Enum => match string_enum_values(schema) {
            Some(values) => arg.value_parser(PossibleValuesParser::new(values)),
            None => arg,
        },
        Kind::Number => arg.value_parser(clap::value_parser!(f64)),
        Kind::Integer => arg.value_parser(clap::value_parser!(i64)),
        Kind::Bool => arg.value_parser(clap::value_parser!(bool)),
        Kind::Json => arg,
    }
}

/// The arg id carrying the shape tag and the property's index.
fn arg_id(shape: Shape, index: usize) -> String {
    let tag = match shape {
        Shape::SetTrue => 'f',
        Shape::Json => Kind::Json.tag(),
        Shape::Value(kind) | Shape::Repeat(kind) => kind.tag(),
    };
    format!("{tag}{index}")
}

/// clap rejects longs holding whitespace or `=`, and anything leading
/// with `-`; the empty long is unusable too.
fn flag_name_ok(long: &str) -> bool {
    !long.is_empty()
        && !long.starts_with('-')
        && !long.chars().any(|c| c.is_whitespace() || c == '=')
}

/// Help text for a repeatable flag, stating so.
/// The flag's help text: the schema description, honest about what the
/// flag demands (`required`, `repeatable`). clap renders neither mark
/// in its usage line — our required flags are
/// `required_unless_present`, so `--json` can carry the payload — and
/// the help text is where the model reads.
fn flag_help(help: Option<&str>, required: bool, repeatable: bool) -> String {
    let mut marks = Vec::new();
    if required {
        marks.push("required");
    }
    if repeatable {
        marks.push("repeatable");
    }
    match help {
        Some(h) if !h.is_empty() => format!("{h} ({})", marks.join(", ")),
        _ => format!("({})", marks.join(", ")),
    }
}

fn with_help(arg: Arg, help: Option<&str>, enforced: bool) -> Arg {
    if enforced {
        return arg.help(flag_help(help, true, false));
    }
    match help {
        Some(h) if !h.is_empty() => arg.help(h.to_owned()),
        _ => arg,
    }
}

/// The schema's top-level properties, in schema order.
fn properties(schema: &Value) -> Option<&Map<String, Value>> {
    schema.get("properties").and_then(Value::as_object)
}

/// The names in the schema's `required` array; non-strings ignored.
fn required_names(schema: &Value) -> Vec<&str> {
    schema
        .get("required")
        .and_then(Value::as_array)
        .map(|names| names.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default()
}

/// Whether the schema pins `default: true` — such a boolean keeps the
/// explicit `true|false` value flag, never a presence guess.
fn default_true(schema: &Value) -> bool {
    schema.get("default") == Some(&Value::Bool(true))
}

/// The flag shape one property maps to, per the schema.
fn classify(schema: &Value) -> Shape {
    // FastMCP's Optional[T]: a two-variant anyOf/oneOf of null and one
    // other shape, unwrapped recursively before classifying.
    if let Some(inner) = nullable_variant(schema) {
        return classify(inner);
    }
    // JSON Schema's ["null", "T"] type array, the same unwrapping.
    if let Some(inner) = nullable_type(schema) {
        return classify(&inner);
    }
    if schema.get("enum").is_some() {
        // A string enum earns possible_values; any other enum falls
        // back to the JSON flag.
        return match string_enum_values(schema) {
            Some(_) => Shape::Value(Kind::Enum),
            None => Shape::Json,
        };
    }
    match schema.get("type").and_then(Value::as_str) {
        Some("string") => Shape::Value(Kind::Text),
        Some("number") => Shape::Value(Kind::Number),
        Some("integer") => Shape::Value(Kind::Integer),
        Some("boolean") => Shape::Value(Kind::Bool),
        Some("array") => array_shape(schema),
        // Objects, unions that survived the nullable unwrap, unknown
        // and missing types all take the JSON escape.
        _ => Shape::Json,
    }
}

/// An array of scalars repeats; anything else falls back to JSON.
fn array_shape(schema: &Value) -> Shape {
    match schema.get("items") {
        Some(items) => match classify(items) {
            Shape::Value(kind) => Shape::Repeat(kind),
            _ => Shape::Json,
        },
        None => Shape::Json,
    }
}

/// The single non-null variant of an `anyOf`/`oneOf` `[T, null]` — the
/// `Optional[T]` shape FastMCP emits for every optional parameter.
fn nullable_variant(schema: &Value) -> Option<&Value> {
    let unions = ["anyOf", "oneOf"]
        .iter()
        .filter_map(|key| schema.get(key).and_then(Value::as_array));
    for variants in unions {
        if variants.len() == 2 {
            if is_null_schema(&variants[0]) {
                return Some(&variants[1]);
            }
            if is_null_schema(&variants[1]) {
                return Some(&variants[0]);
            }
        }
    }
    None
}

/// The schema with a `["null", "T"]` type array rewritten to `"T"`.
fn nullable_type(schema: &Value) -> Option<Value> {
    let types = schema.get("type")?.as_array()?;
    if types.len() != 2 {
        return None;
    }
    let mut others = types.iter().filter(|t| !is_null_schema(t));
    let other = others.next()?;
    if others.next().is_some() {
        return None;
    }
    let mut rewritten = schema.clone();
    rewritten
        .as_object_mut()?
        .insert("type".to_owned(), other.clone());
    Some(rewritten)
}

/// Whether the property itself is nullable, top level: such a flag
/// stays optional even when `required` lists it.
fn is_nullable(schema: &Value) -> bool {
    nullable_variant(schema).is_some() || nullable_type(schema).is_some()
}

/// Whether a value is the JSON `null` schema, in either spelling.
fn is_null_schema(value: &Value) -> bool {
    value.as_str() == Some("null") || value.get("type").and_then(Value::as_str) == Some("null")
}

/// The enum values when the schema is a plain string enum; any
/// non-string value — or a non-string `type` — disqualifies it.
fn string_enum_values(schema: &Value) -> Option<Vec<String>> {
    let string_typed = match schema.get("type") {
        None => true,
        Some(Value::String(t)) => t == "string",
        Some(_) => false,
    };
    if !string_typed {
        return None;
    }
    let values = schema.get("enum")?.as_array()?;
    if values.is_empty() {
        return None;
    }
    values
        .iter()
        .map(|v| v.as_str().map(str::to_owned))
        .collect()
}

/// One flag's contribution to the mapped object, when it was passed.
fn insert_flag(
    object: &mut Map<String, Value>,
    sub: &ArgMatches,
    id: &str,
    long: &str,
    slot: Slot,
    action: &ArgAction,
) {
    let key = match slot {
        Slot::Presence => long.to_owned(),
        Slot::Valued(Kind::Json) => long.strip_suffix("_json").unwrap_or(long).to_owned(),
        Slot::Valued(_) => long.to_owned(),
    };
    match slot {
        Slot::Presence => {
            if matches!(action, ArgAction::SetTrue)
                && let Some(&true) = sub.get_one::<bool>(id)
            {
                object.insert(key, Value::Bool(true));
            }
        }
        Slot::Valued(kind) => {
            let value = match action {
                ArgAction::Append => append_value(sub, id, kind),
                _ => single_value(sub, id, kind),
            };
            if let Some(value) = value {
                object.insert(key, value);
            }
        }
    }
}

/// The mapped value of one passed flag, typed per its kind.
fn single_value(sub: &ArgMatches, id: &str, kind: Kind) -> Option<Value> {
    match kind {
        Kind::Text | Kind::Enum => sub.get_one::<String>(id).map(|v| Value::String(v.clone())),
        Kind::Number => sub
            .get_one::<f64>(id)
            .and_then(|v| Number::from_f64(*v))
            .map(Value::Number),
        Kind::Integer => sub.get_one::<i64>(id).map(|v| Value::Number((*v).into())),
        Kind::Bool => sub.get_one::<bool>(id).map(|v| Value::Bool(*v)),
        Kind::Json => sub.get_one::<Value>(id).cloned(),
    }
}

/// The mapped array of one repeatable flag's occurrences.
fn append_value(sub: &ArgMatches, id: &str, kind: Kind) -> Option<Value> {
    let values = match kind {
        Kind::Text | Kind::Enum => sub
            .get_many::<String>(id)?
            .map(|v| Value::String(v.clone()))
            .collect(),
        Kind::Number => sub
            .get_many::<f64>(id)?
            .filter_map(|v| Number::from_f64(*v))
            .map(Value::Number)
            .collect(),
        Kind::Integer => sub
            .get_many::<i64>(id)?
            .map(|v| Value::Number((*v).into()))
            .collect(),
        Kind::Bool => sub.get_many::<bool>(id)?.map(|v| Value::Bool(*v)).collect(),
        // Arrays of JSON take the JSON flag and never repeat.
        Kind::Json => return None,
    };
    Some(Value::Array(values))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn shape_of(schema: Value) -> Shape {
        classify(&schema)
    }

    #[test]
    fn nullable_anyof_unwraps_recursively() {
        let optional_string = json!({"anyOf": [{"type": "string"}, {"type": "null"}]});
        assert_eq!(shape_of(optional_string), Shape::Value(Kind::Text));
        let optional_array = json!({
            "anyOf": [
                {"type": "array", "items": {"anyOf": [{"type": "integer"}, {"type": "null"}]}},
                {"type": "null"},
            ]
        });
        assert_eq!(shape_of(optional_array), Shape::Repeat(Kind::Integer));
    }

    #[test]
    fn nullable_oneof_and_type_arrays_unwrap() {
        let oneof = json!({"oneOf": [{"type": "null"}, {"type": "boolean"}]});
        assert_eq!(shape_of(oneof), Shape::Value(Kind::Bool));
        let type_array = json!({"type": ["integer", "null"], "default": 7});
        assert_eq!(shape_of(type_array), Shape::Value(Kind::Integer));
    }

    #[test]
    fn three_variant_unions_and_multi_type_arrays_stay_json() {
        let three = json!({"anyOf": [{"type": "object"}, {"type": "string"}, {"type": "null"}]});
        assert_eq!(shape_of(three), Shape::Json);
        let multi = json!({"type": ["string", "number"]});
        assert_eq!(shape_of(multi), Shape::Json);
    }

    #[test]
    fn non_string_enums_and_nested_arrays_stay_json() {
        let numeric_enum = json!({"type": "integer", "enum": [1, 2, 3]});
        assert_eq!(shape_of(numeric_enum), Shape::Json);
        let nested =
            json!({"type": "array", "items": {"type": "array", "items": {"type": "string"}}});
        assert_eq!(shape_of(nested), Shape::Json);
        let items_missing = json!({"type": "array"});
        assert_eq!(shape_of(items_missing), Shape::Json);
    }

    #[test]
    fn cli_safe_is_the_charset_only() {
        assert!(cli_safe("help"));
        assert!(cli_safe("create_pull_request"));
        assert!(!cli_safe(""));
        assert!(!cli_safe("has space"));
        assert!(!cli_safe("dot.name"));
    }
}
