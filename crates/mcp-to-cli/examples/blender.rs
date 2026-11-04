//! A runnable demo of the mapping: a Blender-shaped catalog, rendered.
//!
//! The schemas below mirror what FastMCP servers actually emit (nullable
//! `anyOf` optionals, schema defaults), so the output is what a real
//! `blender` command would look like. Bare invocation prints the full
//! help; `list` prints the bare-invocation JSON; anything after `--` is
//! parsed as argv and printed back as the mapped JSON arguments:
//!
//! ```sh
//! cargo run -p mcp-to-cli --example blender
//! cargo run -p mcp-to-cli --example blender -- list
//! cargo run -p mcp-to-cli --example blender -- get_scene_info --help
//! cargo run -p mcp-to-cli --example blender -- get_scene_info --query chair --fields location --limit 5
//! ```
use mcp_to_cli::{Payload, ToolInfo, args, command, list_json};

fn catalog() -> Vec<ToolInfo> {
    let nullable = |t: &str| serde_json::json!({ "anyOf": [{"type": t}, {"type": "null"}] });
    vec![
        ToolInfo {
            name: "get_scene_info".into(),
            description: "Facts about the scene as text: what's there, where, how big, and \
                          how healthy meshes and rigs are. No image; to see the scene, use look."
                .into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "user_prompt": {"type": "string", "default": ""},
                    "query": nullable("string"),
                    "root": nullable("string"),
                    "fields": {"anyOf": [{"type": "array", "items": {"type": "string"}}, {"type": "null"}]},
                    "limit": {"type": "integer", "default": 20},
                },
            }),
        },
        ToolInfo {
            name: "execute_blender_code".into(),
            description: "Run Python in the user's live Blender (bpy, bmesh, mathutils). \
                          Whatever it prints is returned."
                .into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "code": {"type": "string", "description": "The Python code to execute"},
                    "user_prompt": {"type": "string", "default": ""},
                },
                "required": ["code"],
            }),
        },
        ToolInfo {
            name: "look".into(),
            description: "Grab a viewport screenshot and return it as an image.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "max_size": {"type": "integer", "default": 1000},
                    "user_prompt": {"type": "string", "default": ""},
                },
            }),
        },
    ]
}

fn main() {
    let catalog = catalog();
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let mut cmd = command("blender", &catalog);

    if argv.as_slice() == ["list"] {
        println!("{}", list_json(&catalog));
        return;
    }
    if argv.is_empty() {
        cmd.print_help().expect("help renders to stdout");
        return;
    }

    let mut full = vec!["blender"];
    full.extend(argv.iter().map(String::as_str));
    let matches = cmd
        .clone()
        .try_get_matches_from(full)
        .unwrap_or_else(|e| e.exit());
    match args(&cmd, &matches) {
        Ok(Payload::Mapped(v)) => {
            println!(
                "{}",
                serde_json::to_string_pretty(&v).expect("values render")
            )
        }
        Ok(Payload::Overridden(raw)) => println!("--json override, verbatim: {raw}"),
        Ok(_) => unreachable!("payload variants are the two above"),
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(2);
        }
    }
}
