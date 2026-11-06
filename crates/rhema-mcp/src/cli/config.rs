//! The config-maintenance verbs: `mcp add`, `mcp list`, `mcp remove`.
//!
//! The agent-facing verbs talk to the bridge; these read and write
//! `<config dir>/config.yaml` directly, so they work on the user's own
//! terminal, outside any provisioned shell (no `RHEMA_MCP_BRIDGE` in
//! the environment, which is the point: this is how a server gets
//! configured in the first place). Writes land in the named profile's
//! `mcp_servers` map — `personal` without `--profile` — mirroring the
//! settings screen. The global `mcp.yaml` is never written: it is
//! user-authored, and its formatting and comments are the user's.
//!
//! Shape, modeled on `claude mcp add`: everything before `--` is flags
//! and the server name, everything after `--` is the child's argv.
//!
//! ```text
//! rhema mcp add lightcraft -- /path/to/lightcraft-cli mcp
//! rhema mcp add --profile work jira --url http://127.0.0.1:9721/mcp
//! ```
//!
//! Exit codes join the spec §5 set but not its noise rule, because
//! these verbs face the user, not the model: `0` ok; `2` usage or
//! validation (bad flags, bad name, duplicate without `--force`);
//! `3` the name is not in the profile (`remove`); `1` the config could
//! not be loaded or saved, with the reason on stderr — the call path's
//! "1 prints nothing" rule keeps agent tool output clean, which is not
//! this surface's problem.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use rhema_config::mcp::{McpConfig, McpServerEntry, McpTransport};
use rhema_config::{Config, DEFAULT_PROFILE};

/// Answers `mcp add|list|remove` when the invocation is one of them,
/// `None` for everything else (the bridge verbs keep their dispatch).
/// `argv` is the CLI's own argv, mode first.
pub(crate) fn maybe_run(argv: &[OsString]) -> Option<i32> {
    if is_config_verb(argv) {
        Some(run(&argv[1..]))
    } else {
        None
    }
}

/// Whether the invocation names a config verb. Pure, so the tests can
/// probe the routing without executing a verb against the real config.
fn is_config_verb(argv: &[OsString]) -> bool {
    argv.first().and_then(|a| a.to_str()) == Some("mcp")
        && matches!(
            argv.get(1).and_then(|a| a.to_str()),
            Some("add" | "list" | "remove")
        )
}

/// Runs the verb with the real config path; `1` when there is no home
/// to resolve it against.
fn run(argv: &[OsString]) -> i32 {
    match Config::path() {
        Ok(path) => run_at(&path, argv),
        Err(err) => {
            eprintln!("mcp: {err}");
            1
        }
    }
}

/// The verb's work against an explicit config file — the seam the
/// tests use, since the real path hangs off `$HOME`.
fn run_at(config_path: &Path, argv: &[OsString]) -> i32 {
    let verb = argv.first().and_then(|a| a.to_str()).unwrap_or("");
    let rest = &argv[1.min(argv.len())..];
    match verb {
        "add" => add(config_path, rest),
        "list" => list(config_path, rest),
        "remove" => remove(config_path, rest),
        _ => {
            print_usage();
            0
        }
    }
}

/// The user's global `mcp.yaml`: listed by `list`, checked by `add`
/// for shadowing and by `remove` for the by-hand hint. A sibling of
/// `config.yaml` by definition.
fn global_path(config_path: &Path) -> PathBuf {
    config_path.with_file_name("mcp.yaml")
}

/// Loads the config file, a missing file included: the reason a load
/// can fail is always the caller's to print.
fn load_config(config_path: &Path) -> Result<Config, String> {
    Config::load_from(&config_path.to_path_buf())
        .map_err(|err| format!("reading {}: {err}", config_path.display()))
}

/// Persists the config: the directory `0700` first (it may not exist
/// yet — a first `add` on a fresh machine), the file through
/// [`Config::save_to`]'s atomic `0600` write.
fn save_config(config_path: &Path, config: &Config) -> Result<(), String> {
    if let Some(dir) = config_path.parent() {
        rhema_config::privacy::private_dir(dir)
            .map_err(|err| format!("creating {}: {err}", dir.display()))?;
    }
    Config::save_to(&config_path.to_path_buf(), config).map_err(|err| err.to_string())
}

fn print_usage() {
    println!(
        "usage: rhema mcp add [--profile NAME] <name> -- <command> [args...]\n\
         \x20      rhema mcp add [--profile NAME] <name> --url <URL> [-H 'KEY: VALUE']...\n\
         \n\
         \x20--profile NAME   store the entry on this profile (default: {DEFAULT_PROFILE})\n\
         \x20-e, --env K=V    stdio env var, repeatable\n\
         \x20-H, --header K: V\n\
         \x20                HTTP header, repeatable (with --url)\n\
         \x20-d, --description TEXT\n\
         \x20                prompt disclosure text; omitted, the prompt falls back\n\
         \x20                to \"<name> --help lists its tools\"\n\
         \x20--force          replace an existing entry of the same name\n\
         \x20-h, --help       this text"
    );
}

/// A usage fault: the reason on stderr, exit 2.
fn usage(message: impl std::fmt::Display) -> i32 {
    eprintln!("mcp add: {message}");
    2
}

/// An option's value: glued to the flag with `=`, or the next argv
/// element. `flag` names it in the error.
fn take_value(
    inline: Option<String>,
    iter: &mut std::slice::Iter<'_, OsString>,
    flag: &str,
) -> Result<String, String> {
    inline
        .or_else(|| {
            iter.next()
                .map(|value| value.to_string_lossy().into_owned())
        })
        .ok_or_else(|| format!("`{flag}` needs a value"))
}

/// The parsed `add` line: flags and the server name before `--`, the
/// child's argv after it.
struct AddInvocation {
    profile: Option<String>,
    name: Option<String>,
    description: Option<String>,
    env: BTreeMap<String, String>,
    url: Option<String>,
    headers: BTreeMap<String, String>,
    force: bool,
    command: Option<Vec<String>>,
}

fn parse_add(argv: &[OsString]) -> Result<AddInvocation, i32> {
    let mut parsed = AddInvocation {
        profile: None,
        name: None,
        description: None,
        env: BTreeMap::new(),
        url: None,
        headers: BTreeMap::new(),
        force: false,
        command: None,
    };
    let mut iter = argv.iter();
    while let Some(arg) = iter.next() {
        let text = arg.to_string_lossy().into_owned();
        if text == "--" {
            parsed.command = Some(
                iter.map(|a| a.to_string_lossy().into_owned())
                    .collect::<Vec<_>>(),
            );
            break;
        }
        // `--flag=value` splits; short flags keep their whole text (a
        // value rides separately with them, per the usage line).
        let (flag, inline) = match text.split_once('=') {
            Some((name, value)) if name.starts_with("--") => {
                (name.to_owned(), Some(value.to_owned()))
            }
            _ => (text.clone(), None),
        };
        match flag.as_str() {
            "--force" if inline.is_none() => parsed.force = true,
            "-h" | "--help" if inline.is_none() => {
                print_usage();
                return Err(0);
            }
            "--profile" => match take_value(inline, &mut iter, "--profile") {
                Ok(value) => parsed.profile = Some(value),
                Err(message) => return Err(usage(message)),
            },
            "--url" => match take_value(inline, &mut iter, "--url") {
                Ok(value) => parsed.url = Some(value),
                Err(message) => return Err(usage(message)),
            },
            "-d" | "--description" => match take_value(inline, &mut iter, "--description") {
                Ok(value) => parsed.description = Some(value),
                Err(message) => return Err(usage(message)),
            },
            "-e" | "--env" => match take_value(inline, &mut iter, "--env") {
                Ok(pair) => match pair.split_once('=') {
                    Some((key, value)) if !key.is_empty() => {
                        parsed.env.insert(key.to_owned(), value.to_owned());
                    }
                    _ => return Err(usage(format!("`-e` expects KEY=VALUE, got `{pair}`"))),
                },
                Err(message) => return Err(usage(message)),
            },
            "-H" | "--header" => match take_value(inline, &mut iter, "--header") {
                Ok(pair) => match pair.split_once(':') {
                    Some((key, value)) if !key.trim().is_empty() => {
                        parsed
                            .headers
                            .insert(key.trim().to_owned(), value.trim().to_owned());
                    }
                    _ => return Err(usage(format!("`-H` expects `KEY: VALUE`, got `{pair}`"))),
                },
                Err(message) => return Err(usage(message)),
            },
            _ if text.starts_with('-') && text != "-" => {
                return Err(usage(format!(
                    "unknown flag `{text}`; run `rhema mcp add --help`"
                )));
            }
            _ => {
                if parsed.name.replace(text).is_some() {
                    return Err(usage(format!(
                        "unexpected second name `{}`; put the command after `--`",
                        parsed.name.clone().unwrap_or_default()
                    )));
                }
            }
        }
    }
    Ok(parsed)
}

/// `mcp add`: builds one entry, validates it with the config layer's
/// own rules, and stores it on the profile.
fn add(config_path: &Path, argv: &[OsString]) -> i32 {
    let parsed = match parse_add(argv) {
        Ok(parsed) => parsed,
        Err(code) => return code,
    };
    let AddInvocation {
        profile,
        name,
        description,
        env,
        url,
        headers,
        force,
        command,
    } = parsed;
    let Some(name) = name else {
        return usage("a server name is required: mcp add <name> -- <command...>");
    };
    let profile = profile.unwrap_or_else(|| DEFAULT_PROFILE.to_owned());
    let (command, url) = match (command, url) {
        (Some(_), Some(_)) => {
            return usage("pick one: `-- command...` is stdio, `--url` is HTTP");
        }
        (Some(command), None) if command.is_empty() => {
            return usage("`--` closed the line with no command after it");
        }
        (Some(command), None) => (Some(command), None),
        (None, Some(url)) => (None, Some(url)),
        (None, None) => {
            return usage("one of `-- <command...>` (stdio) or `--url <URL>` (HTTP) is required");
        }
    };
    if !env.is_empty() && url.is_some() {
        return usage("`-e` belongs to stdio servers; use `-H` for `--url`");
    }
    if !headers.is_empty() && command.is_some() {
        return usage("`-H` belongs to `--url` servers; use `-e` for stdio");
    }
    let entry = McpServerEntry {
        description: description.unwrap_or_default(),
        command,
        url,
        env,
        headers,
        startup_timeout: None,
        timeout: None,
        command_name: None,
    };
    // The name charset, the transport XOR, the bash-identifier rules:
    // one validator, the same one every config load applies.
    if let Err(reason) = entry.resolve(&name) {
        return usage(reason);
    }

    let mut config = match load_config(config_path) {
        Ok(config) => config,
        Err(reason) => {
            eprintln!("mcp add: {reason}");
            return 1;
        }
    };
    let profile_is_new = !config.profiles.contains_key(&profile);
    if config
        .effective(Some(&profile))
        .mcp_servers
        .contains_key(&name)
        && !force
    {
        return usage(format!(
            "server `{name}` already exists in profile `{profile}`; pass --force to replace it"
        ));
    }
    let shadowed = McpConfig::load_from(&global_path(config_path))
        .servers
        .iter()
        .any(|server| server.name == name);
    config.set_for_profile(Some(&profile), |target| {
        target.mcp_servers.insert(name.clone(), entry.clone());
    });
    if let Err(reason) = save_config(config_path, &config) {
        eprintln!("mcp add: writing {}: {reason}", config_path.display());
        return 1;
    }

    println!("added `{name}` to profile `{profile}`");
    match (&entry.command, &entry.url) {
        (Some(command), _) => println!("  command: {}", command.join(" ")),
        (_, Some(url)) => println!("  url: {url}"),
        (None, None) => {}
    }
    if entry.description.is_empty() {
        println!(
            "  note: no description; the prompt falls back to \"{name} --help lists its tools\"; \
             add -d to say more"
        );
    }
    if profile_is_new {
        println!("  note: profile `{profile}` did not exist and was created");
    }
    if shadowed {
        println!(
            "  note: the global mcp.yaml also defines `{name}`; this profile entry shadows it"
        );
    }
    0
}

/// One listing row: the resolved transport, rendered flat.
fn resolved_row(name: &str, transport: &McpTransport) -> String {
    match transport {
        McpTransport::Stdio { command, .. } => {
            format!("{name:<16} stdio  {}", command.join(" "))
        }
        McpTransport::Http { url, .. } => format!("{name:<16} http   {url}"),
    }
}

/// The `mcp list` report: the profile's scope and the global file,
/// each as the config layer resolves them on load. An entry that
/// fails validation lists as `invalid` with the reason — surfacing a
/// broken entry is the point of the listing.
fn list_render(config_path: &Path, profile: &str) -> Result<String, String> {
    let config = load_config(config_path)?;
    let stored = config.effective(Some(profile));
    let global = McpConfig::load_from(&global_path(config_path));
    if stored.mcp_servers.is_empty() && global.servers.is_empty() {
        return Ok(format!(
            "nothing configured for profile `{profile}`\n  \
             add one: rhema mcp add <name> -- <command> [args...]"
        ));
    }
    let mut out = String::new();
    if !stored.mcp_servers.is_empty() {
        out.push_str(&format!("profile {profile}:\n"));
        for (name, entry) in &stored.mcp_servers {
            let row = match entry.resolve(name) {
                Ok(server) => resolved_row(&server.name, &server.transport),
                Err(reason) => format!("{name:<16} invalid  {reason}"),
            };
            out.push_str("  ");
            out.push_str(&row);
            out.push('\n');
        }
    }
    if !global.servers.is_empty() {
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str("global mcp.yaml:\n");
        for server in &global.servers {
            out.push_str("  ");
            out.push_str(&resolved_row(&server.name, &server.transport));
            out.push('\n');
        }
    }
    Ok(out)
}

/// `mcp list`: renders the report for one profile (the default
/// without `--profile`).
fn list(config_path: &Path, argv: &[OsString]) -> i32 {
    let mut profile: Option<String> = None;
    let mut pending_profile = false;
    for arg in argv {
        let text = arg.to_string_lossy().into_owned();
        if pending_profile {
            profile = Some(text);
            pending_profile = false;
            continue;
        }
        match text.as_str() {
            "-h" | "--help" => {
                print_usage();
                return 0;
            }
            "--profile" => pending_profile = true,
            _ if text.starts_with('-') && text != "-" => {
                eprintln!("mcp list: unknown flag `{text}`");
                return 2;
            }
            _ => {
                eprintln!("mcp list: unexpected argument `{text}`");
                return 2;
            }
        }
    }
    if pending_profile {
        eprintln!("mcp list: `--profile` needs a value");
        return 2;
    }
    let profile = profile.unwrap_or_else(|| DEFAULT_PROFILE.to_owned());
    match list_render(config_path, &profile) {
        Ok(report) => {
            println!("{}", report.trim_end());
            0
        }
        Err(reason) => {
            eprintln!("mcp list: {reason}");
            1
        }
    }
}

/// `mcp remove`: takes the name off the profile's map. A name the
/// profile never held is exit 3, with the global file named when it is
/// the one holding the entry.
fn remove(config_path: &Path, argv: &[OsString]) -> i32 {
    let mut profile = None;
    let mut name = None;
    let mut iter = argv.iter();
    while let Some(arg) = iter.next() {
        let text = arg.to_string_lossy().into_owned();
        match text.as_str() {
            "-h" | "--help" => {
                print_usage();
                return 0;
            }
            "--profile" => match iter.next() {
                Some(value) => profile = Some(value.to_string_lossy().into_owned()),
                None => {
                    eprintln!("mcp remove: `--profile` needs a value");
                    return 2;
                }
            },
            _ if text.starts_with('-') && text != "-" => {
                eprintln!("mcp remove: unknown flag `{text}`");
                return 2;
            }
            _ => {
                if name.replace(text.clone()).is_some() {
                    eprintln!("mcp remove: unexpected second name `{text}`");
                    return 2;
                }
            }
        }
    }
    let Some(name) = name else {
        eprintln!("usage: mcp remove [--profile NAME] <name>");
        return 2;
    };
    let profile = profile.unwrap_or_else(|| DEFAULT_PROFILE.to_owned());
    let mut config = match load_config(config_path) {
        Ok(config) => config,
        Err(reason) => {
            eprintln!("mcp remove: {reason}");
            return 1;
        }
    };
    let removed = config
        .profiles
        .get_mut(&profile)
        .map(|target| target.mcp_servers.remove(&name))
        .unwrap_or_default();
    if removed.is_none() {
        let in_global = McpConfig::load_from(&global_path(config_path))
            .servers
            .iter()
            .any(|server| server.name == name);
        if in_global {
            eprintln!(
                "mcp remove: `{name}` is not in profile `{profile}`; it is defined in the global \
                 mcp.yaml, which this command never edits — remove it there by hand"
            );
        } else {
            eprintln!("mcp remove: no server `{name}` in profile `{profile}`");
        }
        return 3;
    }
    if let Err(reason) = save_config(config_path, &config) {
        eprintln!("mcp remove: writing {}: {reason}", config_path.display());
        return 1;
    }
    println!("removed `{name}` from profile `{profile}`");
    println!("  note: running sessions keep the old wiring until they restart");
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A config file in a fresh temp dir, plus the global sibling.
    fn setup() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = dir.path().join("config.yaml");
        let global = dir.path().join("mcp.yaml");
        (dir, config, global)
    }

    fn run(config: &Path, args: &[&str]) -> i32 {
        run_at(config, &args.iter().map(OsString::from).collect::<Vec<_>>())
    }

    #[test]
    fn add_stores_a_stdio_entry_on_the_default_profile() {
        let (_dir, config, _global) = setup();
        assert_eq!(
            run(&config, &["add", "lightcraft", "--", "/bin/echo", "mcp"]),
            0
        );
        let stored = Config::load_from(&config).expect("loads");
        let entry = &stored.effective(None).mcp_servers["lightcraft"];
        assert_eq!(
            entry.command,
            Some(vec!["/bin/echo".to_owned(), "mcp".to_owned()])
        );
        assert!(entry.url.is_none());
        assert_eq!(entry.description, "");
    }

    #[test]
    fn add_honors_profile_env_description_and_http() {
        let (_dir, config, _global) = setup();
        assert_eq!(
            run(
                &config,
                &[
                    "add",
                    "--profile=work",
                    "-d",
                    "Jira and Confluence",
                    "-e",
                    "JIRA_TOKEN=t",
                    "atlassian",
                    "--",
                    "npx",
                    "-y",
                    "server",
                ]
            ),
            0
        );
        assert_eq!(
            run(
                &config,
                &[
                    "add",
                    "blender",
                    "--url=http://127.0.0.1:9721/mcp",
                    "-H",
                    "X-A: b"
                ]
            ),
            0
        );
        let stored = Config::load_from(&config).expect("loads");
        let atlassian = &stored.effective(Some("work")).mcp_servers["atlassian"];
        assert_eq!(atlassian.description, "Jira and Confluence");
        assert_eq!(
            atlassian.env.get("JIRA_TOKEN").map(String::as_str),
            Some("t")
        );
        let blender = &stored.effective(None).mcp_servers["blender"];
        assert_eq!(blender.url.as_deref(), Some("http://127.0.0.1:9721/mcp"));
        assert_eq!(blender.headers.get("X-A").map(String::as_str), Some("b"));
        // The stored entries all resolve: the CLI wrote valid config.
        for profile in [stored.effective(Some("work")), stored.effective(None)] {
            for (name, entry) in &profile.mcp_servers {
                assert!(entry.resolve(name).is_ok(), "{name}: resolves");
            }
        }
    }

    #[test]
    fn add_rejects_duplicates_until_forced() {
        let (_dir, config, _global) = setup();
        assert_eq!(run(&config, &["add", "x", "--", "true"]), 0);
        assert_eq!(run(&config, &["add", "x", "--", "false"]), 2);
        let stored = Config::load_from(&config).expect("loads");
        assert_eq!(
            stored.effective(None).mcp_servers["x"].command,
            Some(vec!["true".to_owned()]),
            "the failed add did not touch the entry"
        );
        assert_eq!(run(&config, &["add", "--force", "x", "--", "false"]), 0);
        let stored = Config::load_from(&config).expect("loads");
        assert_eq!(
            stored.effective(None).mcp_servers["x"].command,
            Some(vec!["false".to_owned()])
        );
    }

    #[test]
    fn add_rejects_bad_names_and_missing_or_double_transports() {
        let (_dir, config, _global) = setup();
        assert_eq!(run(&config, &["add", "Big Name", "--", "true"]), 2);
        assert_eq!(run(&config, &["add", "nameless"]), 2);
        assert_eq!(
            run(&config, &["add", "both", "--url", "u", "--", "true"]),
            2
        );
        assert_eq!(run(&config, &["add", "empty", "--"]), 2);
        assert_eq!(run(&config, &["add"]), 2);
        assert_eq!(run(&config, &["add", "n", "--frobnicate"]), 2);
        assert_eq!(run(&config, &["add", "n", "--url", "u", "-e", "K=V"]), 2);
        assert_eq!(run(&config, &["add", "n", "-H", "K: V", "--", "c"]), 2);
        assert_eq!(run(&config, &["add", "n", "c1", "c2"]), 2);
        // Nothing was stored by any of the failures.
        let stored = Config::load_from(&config).expect("loads");
        assert!(stored.effective(None).mcp_servers.is_empty());
    }

    #[test]
    fn add_creates_an_unknown_profile() {
        let (_dir, config, _global) = setup();
        assert_eq!(
            run(&config, &["add", "--profile", "farm", "x", "--", "true"]),
            0
        );
        let stored = Config::load_from(&config).expect("loads");
        assert!(stored.profiles["farm"].mcp_servers.contains_key("x"));
    }

    #[test]
    fn list_renders_both_scopes_and_invalid_entries() {
        let (_dir, config, global) = setup();
        std::fs::write(
            &global,
            "servers:\n  blender:\n    url: \"http://127.0.0.1:9721/mcp\"\n",
        )
        .expect("writes global");
        assert_eq!(run(&config, &["add", "good", "--", "echo", "hi"]), 0);
        let text = list_render(&config, "personal").expect("renders");
        assert!(text.contains("profile personal:"), "{text}");
        assert!(text.contains("good"), "{text}");
        assert!(text.contains("global mcp.yaml:"), "{text}");
        assert!(text.contains("http://127.0.0.1:9721/mcp"), "{text}");
        // A stored entry that fails its resolve lists as invalid.
        std::fs::write(
            &config,
            "profiles:\n  personal:\n    mcp_servers:\n      broken:\n        url: \" \"\n",
        )
        .expect("writes config");
        let text = list_render(&config, "personal").expect("renders");
        assert!(text.contains("invalid"), "{text}");
        // An empty machine names the way out instead.
        let (_dir, fresh, _global) = setup();
        let text = list_render(&fresh, "personal").expect("renders");
        assert!(text.contains("nothing configured"), "{text}");
        assert!(text.contains("rhema mcp add"), "{text}");
    }

    #[test]
    fn remove_deletes_then_reports_unknown_with_the_global_hint() {
        let (_dir, config, global) = setup();
        assert_eq!(run(&config, &["add", "x", "--", "true"]), 0);
        assert_eq!(run(&config, &["remove", "x"]), 0);
        let stored = Config::load_from(&config).expect("loads");
        assert!(stored.effective(None).mcp_servers.is_empty());
        // Unknown in the profile, present in the global file: named.
        std::fs::write(&global, "servers:\n  g:\n    command: [true]\n").expect("writes global");
        assert_eq!(run(&config, &["remove", "g"]), 3);
        // Unknown everywhere: still exit 3.
        assert_eq!(run(&config, &["remove", "nope"]), 3);
    }

    #[test]
    fn the_router_takes_only_the_config_verbs() {
        // Pure routing: an execution here would touch the real
        // $HOME config, which no test may do.
        let mcp = |args: &[&str]| -> Vec<OsString> {
            std::iter::once("mcp")
                .chain(args.iter().copied())
                .map(OsString::from)
                .collect()
        };
        assert!(is_config_verb(&mcp(&["add", "x", "--", "true"])));
        assert!(is_config_verb(&mcp(&["list"])));
        assert!(is_config_verb(&mcp(&["remove", "x"])));
        assert!(!is_config_verb(&mcp(&["servers"])));
        assert!(!is_config_verb(&mcp(&["tools", "add"])));
        assert!(!is_config_verb(&mcp(&["call", "add", "t", "{}"])));
        assert!(!is_config_verb(&[
            OsString::from("__server"),
            OsString::from("x")
        ]));
        assert!(!is_config_verb(&[OsString::from("mcp")]));
    }

    #[test]
    fn help_exits_ok() {
        let (_dir, config, _global) = setup();
        assert_eq!(run(&config, &["add", "--help"]), 0);
        assert_eq!(run(&config, &["list", "--help"]), 0);
        assert_eq!(run(&config, &["remove", "--help"]), 0);
        assert_eq!(run(&config, &[]), 0);
    }
}
