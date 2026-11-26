//! The `__search` mode and the session wiring: the `search` macro's
//! other end.
//!
//! The agent's shell execs `rhema __search "query"`; this module owns
//! the pass-through: it parses the flags, resolves the profile the
//! wiring named, assembles that profile's chain through the registry,
//! runs the query, and prints the markdown. The settings are read
//! fresh per call, so a provider added mid-session serves the next
//! `search` without a restart; the profile is the wiring's env var,
//! pinned at session start, the one per-session input a standalone
//! exec can carry without session state.
//!
//! A profile with no providers wires nothing, no function and no
//! prompt section (`specs/rhema-websearch.md` §8). A sandboxed session
//! wires only when the sandbox has the installed helper and the
//! profile opts into credential injection: the helper runs the mode,
//! the host supplies the resolved chain. One wiring per session key,
//! held for the process, because the
//! init file's env map is per-profile and a profile switch re-runs
//! the assembly: the same shape the MCP registry keeps.

#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::sync::{Arc, Mutex, OnceLock};

use rhema_config::Profile;
use rhema_config::secrets::SecretStore;

/// The resolved chain the host hands a sandboxed helper: a JSON array
/// of `{name, provider, key}`, the profile resolved host-side. The
/// local mode never sees it; its config and keychain are here.
const VAR_CHAIN: &str = "RHEMA_SEARCH_CHAIN";

/// The usage text the mode prints on stderr, and the model reads, on
/// a query it cannot serve as typed.
const USAGE: &str = "search: web search through this session's providers

    search \"query text\"          general index
    search -n 5 \"query\"          cap the results (default 8, max 20)
    search --news \"query\"        the news index

    exit 0: results printed      exit 1: every provider failed
    exit 2: bad usage            exit 4: not wired in this shell";

/// One session's wiring, the sandbox wiring shape: the provisioned init
/// file held for the process's life, one per session key.
static WIRED: OnceLock<Mutex<BTreeMap<String, Arc<rhema_bash::websearch::Wiring>>>> =
    OnceLock::new();

fn registry() -> &'static Mutex<BTreeMap<String, Arc<rhema_bash::websearch::Wiring>>> {
    WIRED.get_or_init(|| Mutex::new(BTreeMap::new()))
}

/// Wires the session's search macro, the setup's call: the gate first
/// (a sandboxed session and a providerless profile both wire
/// nothing), then one provision per session key. `None` when the gate
/// closes: the caller renders no section, the agent sees no function.
pub(crate) fn wire_for_session(
    session_key: &str,
    profile: &Profile,
    secrets: Option<&dyn SecretStore>,
    sandboxed: Option<&crate::sandbox::Sandboxed>,
    bash_env: Option<std::ffi::OsString>,
) -> Option<Arc<rhema_bash::websearch::Wiring>> {
    // The gate answers the only question the settings can: does this
    // profile have providers to try? The chain itself is not cached;
    // the mode handler assembles fresh per invocation, so added
    // providers land without a restart.
    let selected = rhema_websearch_registry::selections(Some(profile), secrets);
    if selected.is_empty() {
        return None;
    }
    // The injected route needs the opt-in and an installed helper.
    // A mounted sandbox has one under the shared root; a sandbox with
    // no root has one in its own cache.
    let inject =
        sandboxed.is_none_or(|s| s.helper_available()) && profile.sandbox_inject_credentials();
    if sandboxed.is_some() && !inject {
        return None;
    }
    let mut reg = registry().lock().expect("search wire registry lock");
    if let Some(wiring) = reg.get(session_key) {
        return Some(wiring.clone());
    }
    let bin = std::env::current_exe().ok()?;
    let mut wiring = rhema_bash::websearch::provision_with_bin(bash_env, &bin, session_key).ok()?;
    if inject {
        // The profile resolved here; the sandbox gets the instances
        // and their keys, not a config file it cannot read.
        let payload: Vec<serde_json::Value> = selected
            .iter()
            .map(|(name, provider, key)| {
                serde_json::json!({"name": name, "provider": provider, "key": key})
            })
            .collect();
        wiring
            .shell_env
            .insert(VAR_CHAIN.to_owned(), serde_json::to_string(&payload).ok()?);
    }
    let wiring = Arc::new(wiring);
    reg.insert(session_key.to_owned(), wiring.clone());
    Some(wiring)
}

/// One parsed invocation: the query text and the two flags. `parse`
/// is the mode's whole argv contract, `run` its whole behavior, so
/// both are testable without a process or a secret store.
#[derive(Clone)]
pub(crate) struct Invocation {
    /// The query text, the flags-free remainder joined with spaces.
    pub query: rhema_websearch::SearchQuery,
}

/// Parses the mode's argv (mode token already skipped). A usage error
/// is [`rhema_bash::websearch::EXIT_USAGE`]; the caller prints
/// [`USAGE`].
pub(crate) fn parse(args: &[OsString]) -> Result<Invocation, ()> {
    let mut news = false;
    let mut count: Option<u8> = None;
    let mut words: Vec<String> = Vec::new();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.to_string_lossy().as_ref() {
            "--news" => news = true,
            "-n" => {
                let Some(value) = iter.next() else {
                    return Err(());
                };
                // Parse wide, then clamp: `-n 300` asks for more than
                // the seam allows, which the query clamps to 20, not
                // a usage error.
                let Ok(parsed) = value.to_string_lossy().parse::<u32>() else {
                    return Err(());
                };
                count = Some(u8::try_from(parsed).unwrap_or(u8::MAX));
            }
            other if other.starts_with('-') => return Err(()),
            other => words.push(other.to_owned()),
        }
    }
    let text = words.join(" ");
    if text.trim().is_empty() {
        return Err(());
    }
    let mut query = rhema_websearch::SearchQuery::new(text);
    if let Some(count) = count {
        query = query.with_max_results(count);
    }
    Ok(Invocation {
        query: query.with_news(news),
    })
}

/// Runs one invocation against a caller-supplied chain and prints the
/// answer: the markdown on stdout, the failure's own text on stderr,
/// the exit codes the usage table names. The seam the CLI tests
/// drive; `cli_run` is this plus the process bits.
///
/// The future runs on its own thread: the binary's `main` is
/// `#[tokio::main]`, so this sync mode can be entered from inside a
/// runtime (the headless path, which is where the shell function
/// finds it), and a runtime built or blocked on right here would
/// panic with the nested-runtime error. A fresh thread is outside
/// any runtime, whichever side spawns the mode.
pub(crate) fn run(invocation: &Invocation, chain: &rhema_websearch::Chain) -> i32 {
    let invocation = invocation.clone();
    let chain = chain.clone();
    let outcome = std::thread::scope(|scope| {
        scope
            .spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("a current-thread runtime cannot fail to build");
                runtime.block_on(chain.search(&invocation.query))
            })
            .join()
            .expect("the search thread carries no panic of its own")
    });
    match outcome {
        Ok(outcome) => {
            println!("{}", rhema_websearch::render(&outcome));
            0
        }
        Err(err) => {
            eprintln!("search: {err}");
            1
        }
    }
}

/// The mode's entry: the process exit code is ours, its stderr is the
/// model's error text, the vision and jobs modes' contract.
pub fn cli_run(argv: &[OsString]) -> i32 {
    // Parse first: a usage error needs no environment and no config,
    // and it is the one exit code a test can pin without either.
    let invocation = match parse(&argv.iter().skip(1).cloned().collect::<Vec<_>>()) {
        Ok(invocation) => invocation,
        Err(()) => {
            eprintln!("{USAGE}");
            return rhema_bash::websearch::EXIT_USAGE;
        }
    };
    // The sandboxed helper: the host resolved the chain and handed it
    // over as env. No config file, no keychain, no profile lookup.
    if let Some(chain) = injected_chain() {
        return run(&invocation, &chain);
    }
    let Some(profile_name) = std::env::var(rhema_bash::websearch::VAR_PROFILE)
        .ok()
        .filter(|name| !name.trim().is_empty())
    else {
        eprintln!(
            "search: not wired in this shell (no {})",
            rhema_bash::websearch::VAR_PROFILE
        );
        return rhema_bash::websearch::EXIT_NOT_WIRED;
    };
    // The stored settings, the headless run's convention: an
    // unreadable file is the default document, never an error wall.
    let doc = crate::store::Config::load().unwrap_or_default();
    // The named profile, as the wiring named it at session start; a
    // name the config lost reads as the default profile, the same
    // leniency `effective` applies to an absent flag.
    let profile = doc.effective(Some(&profile_name));
    let secrets = rhema_config::Home::discover()
        .ok()
        .map(|home| home.secret_store());
    let Some(chain) = rhema_websearch_registry::assemble_chain(Some(&profile), secrets.as_deref())
    else {
        eprintln!(
            "search: no provider is configured for profile \"{profile_name}\" (settings \
             > Providers > Web search, or a BRAVE_SEARCH_API_KEY / EXA_API_KEY / TAVILY_API_KEY \
             environment variable)"
        );
        return 1;
    };
    run(&invocation, &chain)
}

/// One host-resolved entry, the shape [`VAR_CHAIN`] carries.
#[derive(serde::Deserialize)]
struct InjectedEntry {
    name: String,
    provider: String,
    key: String,
}

/// The chain the host handed this process, when [`VAR_CHAIN`] holds
/// one. A malformed value falls through to the local path, which then
/// reports the missing config; the host's serializer cannot produce
/// one, so this is the corrupt-env case, not a settings error.
fn injected_chain() -> Option<rhema_websearch::Chain> {
    chain_from_injected(&std::env::var(VAR_CHAIN).ok()?)
}

/// [`injected_chain`] over the raw value, the seam tests drive.
fn chain_from_injected(raw: &str) -> Option<rhema_websearch::Chain> {
    let entries: Vec<InjectedEntry> = serde_json::from_str(raw).ok()?;
    if entries.is_empty() {
        return None;
    }
    let mut chain = rhema_websearch::Chain::new();
    for entry in entries {
        let provider = rhema_websearch_registry::build(&entry.provider, Some(entry.key)).ok()?;
        chain.push(entry.name, provider);
    }
    Some(chain)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rhema_websearch::mock::MockSearchProvider;

    fn os(args: &[&str]) -> Vec<OsString> {
        args.iter().map(OsString::from).collect()
    }

    #[test]
    fn the_query_is_the_flag_free_remainder() {
        let invocation = parse(&os(&["rust", "async", "book"])).unwrap();
        assert_eq!(invocation.query.text, "rust async book");
        assert_eq!(
            invocation.query.max_results,
            rhema_websearch::query::DEFAULT_MAX_RESULTS
        );
        assert!(!invocation.query.news);
    }

    #[test]
    fn the_flags_are_count_and_news() {
        let invocation = parse(&os(&["-n", "5", "--news", "rust", "release"])).unwrap();
        assert_eq!(invocation.query.max_results, 5);
        assert!(invocation.query.news);
        assert_eq!(invocation.query.text, "rust release");
    }

    #[test]
    fn a_lone_or_bad_count_is_usage() {
        assert!(parse(&os(&["-n"])).is_err());
        assert!(parse(&os(&["-n", "many", "q"])).is_err());
    }

    #[test]
    fn an_unknown_flag_and_an_empty_query_are_usage() {
        assert!(parse(&os(&["--deep", "q"])).is_err());
        assert!(parse(&os(&["-x"])).is_err());
        assert!(parse(&os(&[])).is_err());
        assert!(parse(&os(&[""])).is_err(), "blank is no query");
        assert!(parse(&os(&["  ", "\t"])).is_err(), "whitespace is no query");
    }

    #[test]
    fn an_over_large_count_clamps_instead_of_failing() {
        let invocation = parse(&os(&["-n", "300", "q"])).unwrap();
        assert_eq!(invocation.query.max_results, 20);
        let invocation = parse(&os(&["-n", "0", "q"])).unwrap();
        assert_eq!(invocation.query.max_results, 1);
    }

    #[test]
    fn a_query_of_flags_only_is_usage() {
        assert!(parse(&os(&["--news"])).is_err(), "no query text, no run");
    }

    #[test]
    fn the_run_prints_the_win_and_fails_with_the_ladder() {
        let winner = MockSearchProvider::new("brave").shared();
        winner.push_ok(vec![rhema_websearch::SearchResult::new(
            "T",
            "https://x",
            "snippet",
        )]);
        let mut chain = rhema_websearch::Chain::new();
        chain.push("brave-main", winner);
        let invocation = parse(&os(&["rust"])).unwrap();
        assert_eq!(run(&invocation, &chain), 0);

        let loser = MockSearchProvider::new("exa").shared();
        loser.push_err(rhema_websearch::SearchError::RateLimited);
        let mut chain = rhema_websearch::Chain::new();
        chain.push("exa-work", loser);
        assert_eq!(run(&invocation, &chain), 1);
    }

    #[test]
    fn the_wiring_gate_closes_on_sandbox_and_on_no_providers() {
        let profile = Profile::default();
        assert!(
            wire_for_session("personal", &profile, None, None, None).is_none(),
            "no providers, no wiring"
        );
        // The sandbox case needs a Sandboxed handle, the assembly's
        // own; its None arm is the same early return the providerless
        // case exercises, and app tests cover the gated path.
    }

    #[test]
    fn the_registry_holds_one_wiring_per_session_key() {
        // Direct provision, not wire_for_session: the registry path
        // needs current_exe to speak MODE, which a test binary does.
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("rhema-fake");
        std::fs::write(&bin, "#!/bin/sh\nexit 0\n").unwrap();
        let first =
            rhema_bash::websearch::provision_with_bin(None, &bin, "registry-test-a").unwrap();
        let second =
            rhema_bash::websearch::provision_with_bin(None, &bin, "registry-test-b").unwrap();
        assert_ne!(
            first.init_path, second.init_path,
            "two session keys, two init files"
        );
        assert_eq!(
            first
                .shell_env
                .get(rhema_bash::websearch::VAR_PROFILE)
                .map(String::as_str),
            Some("registry-test-a")
        );
    }
}

#[cfg(test)]
mod runtime_tests {
    use super::*;
    use rhema_websearch::mock::MockSearchProvider;

    /// The regression: the binary's main is #[tokio::main], and the
    /// headless path enters this mode from inside that runtime. A
    /// runtime of the mode's own, built or blocked on here, panics
    /// with the nested-runtime error before a single provider is
    /// asked. The worker-thread escape survives the same context —
    /// and the plain #[test]s already pin the outside-a-runtime
    /// side, so both shapes of caller stay covered.
    #[tokio::test]
    async fn a_run_inside_a_runtime_does_not_panic() {
        let winner = MockSearchProvider::new("exa").shared();
        let mut chain = rhema_websearch::Chain::new();
        chain.push("Exa", winner);
        let invocation = Invocation {
            query: rhema_websearch::SearchQuery::new("tokio vs smol"),
        };
        let code = tokio::task::spawn_blocking(move || run(&invocation, &chain))
            .await
            .unwrap();
        assert_eq!(code, 0, "the mock answers, the mode exits clean");
    }
}

#[cfg(test)]
mod sandbox_tests {
    use super::*;
    use crate::args::SandboxChoice;
    use crate::sandbox::Sandboxed;
    use rhema_bash::ShellTools;
    use rhema_config::{SandboxSettings, WebSearchInstance};
    use std::sync::Arc;

    fn sandboxed(shared: bool) -> (Sandboxed, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let sandboxed = Sandboxed {
            choice: SandboxChoice::Docker,
            target: "test".to_owned(),
            tools: Arc::new(ShellTools::new("/workspace")),
            workspace: "/workspace".to_owned(),
            skills: Vec::new(),
            instance_lock: None,
            managed: None,
            shared: shared.then(|| rhema_sandbox::kit::SharedRoot::new(dir.path())),
            mcp: crate::mcp_config::McpStore::local(None, &std::collections::BTreeSet::new()),
            session: Default::default(),
            sandbox: None,
            remote: None,
            remote_session: Default::default(),
        };
        (sandboxed, dir)
    }

    fn profile(inject: bool) -> Profile {
        let mut profile = Profile {
            sandbox: Some(SandboxSettings {
                inject_credentials: inject,
                ..SandboxSettings::default()
            }),
            ..Profile::default()
        };
        profile.websearch.insert(
            "exa-work".into(),
            WebSearchInstance {
                provider: "exa".into(),
                api_key: Some("literal-key".into()),
                secret_ref: None,
            },
        );
        profile.websearch_order = vec!["exa-work".into()];
        profile
    }

    fn remote_sandbox() -> Sandboxed {
        Sandboxed {
            choice: SandboxChoice::Ssh,
            target: "deploy@box".to_owned(),
            tools: Arc::new(ShellTools::new("/workspace")),
            workspace: "/workspace".to_owned(),
            skills: Vec::new(),
            instance_lock: None,
            managed: None,
            shared: None,
            mcp: crate::mcp_config::McpStore::local(None, &std::collections::BTreeSet::new()),
            session: Default::default(),
            sandbox: None,
            remote: Some(rhema_sandbox::kit::SandboxKit {
                helper: "/home/sandbox/.cache/rhema/kit/3/rhema".into(),
                helper_dir: "/home/sandbox/.cache/rhema/kit/3".into(),
                init: "/home/sandbox/.cache/rhema/runtime/init.sh".into(),
                root: "/home/sandbox/.cache/rhema/runtime".into(),
                gob: true,
            }),
            remote_session: Default::default(),
        }
    }

    #[test]
    fn an_ssh_sandbox_with_the_installed_helper_wires_the_chain() {
        let sandboxed = remote_sandbox();
        let wiring = wire_for_session(
            "sandbox-search-ssh-inject",
            &profile(true),
            None,
            Some(&sandboxed),
            None,
        )
        .expect("an opted-in ssh session wires");
        let raw = wiring.shell_env.get(VAR_CHAIN).expect("the chain env");
        assert!(raw.contains("literal-key"), "{raw}");
    }

    #[test]
    fn the_resolved_chain_crosses_in_env_when_opted_in() {
        let (sandboxed, _dir) = sandboxed(true);
        let wiring = wire_for_session(
            "sandbox-search-inject",
            &profile(true),
            None,
            Some(&sandboxed),
            None,
        )
        .expect("the opted-in session wires");
        let raw = wiring.shell_env.get(VAR_CHAIN).expect("the chain env");
        assert!(raw.contains("exa-work"), "{raw}");
        assert!(raw.contains("literal-key"), "{raw}");
    }

    #[test]
    fn without_the_opt_in_a_sandboxed_session_does_not_wire() {
        let (sandboxed, _dir) = sandboxed(true);
        assert!(
            wire_for_session(
                "sandbox-search-no-inject",
                &profile(false),
                None,
                Some(&sandboxed),
                None,
            )
            .is_none()
        );
    }
}

#[cfg(test)]
mod injected_tests {
    use super::chain_from_injected;

    #[test]
    fn a_host_resolved_chain_builds() {
        let raw = r#"[{"name":"exa-work","provider":"exa","key":"k"}]"#;
        assert!(chain_from_injected(raw).is_some());
    }

    #[test]
    fn an_empty_or_malformed_chain_falls_through() {
        assert!(chain_from_injected("[]").is_none());
        assert!(chain_from_injected("{}").is_none());
        assert!(chain_from_injected("not json").is_none());
    }
}
