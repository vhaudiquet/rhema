//! A stress harness for the mapping: build one command tree from a
//! directory of real MCP tool definitions (wire-format JSON objects
//! with `name`, `description`, `inputSchema` — e.g. github-mcp-server's
//! `__toolsnaps__`), then behave like the other examples. Prints a
//! render summary to stderr:
//!
//! ```sh
//! cargo run -p mcp-to-cli --example catalog -- /path/to/__toolsnaps__
//! cargo run -p mcp-to-cli --example catalog -- /path/to/__toolsnaps__ -- list
//! cargo run -p mcp-to-cli --example catalog -- /path/to/__toolsnaps__ -- create_issue --help
//! cargo run -p mcp-to-cli --example catalog -- /path/to/__toolsnaps__ -- create_issue --owner acme --repo api --title hi
//! ```
use mcp_to_cli::{Payload, ToolInfo, args, command, list_json};

fn main() {
    let mut argv = std::env::args().skip(1);
    let dir = argv.next().unwrap_or_else(|| {
        eprintln!("usage: catalog <dir-of-tool-json> [-- argv]");
        std::process::exit(2);
    });
    let argv: Vec<String> = argv.collect();

    let mut paths: Vec<_> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("{dir}: {e}"))
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|e| e == "json" || e == "snap"))
        .collect();
    paths.sort();
    let catalog: Vec<ToolInfo> = paths
        .iter()
        .map(|p| {
            let raw: serde_json::Value = std::fs::read_to_string(p)
                .expect("fixture reads")
                .parse()
                .expect("tool JSON parses");
            let field = |k: &str| {
                raw.get(k)
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default()
                    .to_owned()
            };
            ToolInfo {
                name: field("name"),
                description: field("description"),
                input_schema: raw.get("inputSchema").cloned().unwrap_or_default(),
            }
        })
        .collect();
    eprintln!(
        "{} tools loaded, {} rendered as subcommands",
        catalog.len(),
        command("srv", &catalog).get_subcommands().count() - 1, // minus `help`
    );

    let mut cmd = command("srv", &catalog);
    if argv.as_slice() == ["list"] {
        println!("{}", list_json(&catalog));
        return;
    }
    if argv.is_empty() {
        cmd.print_help().expect("help renders to stdout");
        return;
    }
    let mut full = vec!["srv"];
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
