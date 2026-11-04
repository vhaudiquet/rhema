//! A runnable demo of the required-argument story: the real GitHub
//! `create_pull_request` schema (five required of nine properties), so
//! the help output is what a real `github` command would look like.
//! Same driver as the Blender example: bare prints help, argv maps.
use mcp_to_cli::{Payload, ToolInfo, args, command};

fn main() {
    let schema = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/create_pull_request.json"
    ));
    let catalog = [ToolInfo {
        name: "create_pull_request".into(),
        description: "Create a new pull request in a GitHub repository".into(),
        input_schema: serde_json::from_str(schema).expect("fixture parses"),
    }];
    let mut cmd = command("github", &catalog);

    let argv: Vec<String> = std::env::args().skip(1).collect();
    if argv.is_empty() {
        cmd.print_help().expect("help renders to stdout");
        return;
    }
    let mut full = vec!["github"];
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
