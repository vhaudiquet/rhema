//! The `__decision` mode and the session wiring: the `decide` macro's
//! other end.
//!
//! The agent's shell calls the session's `decide` function. A local
//! shell execs `rhema __decision …`; a sandboxed one posts the argv
//! and any stdin to the host endpoint in [`crate::cap`]. Both paths
//! share the same parser, the same provider resolution, and the same
//! renderer.
//!
//! A profile naming no decision provider wires nothing, no function
//! and no prompt section (`specs/rhema-decision.md` §10). The host
//! resolves the profile's instance once, at session setup, and keeps
//! it in this process. The sandbox sees the session socket and its
//! token, never the key.

#![forbid(unsafe_code)]

use std::ffi::OsString;
use std::io::Read as _;
use std::sync::Arc;

use rhema_config::secrets::SecretStore;
use rhema_config::{DecisionInstance, Profile};
use rhema_decision::{DecisionError, DecisionProvider, DecisionRequest, Question};

/// The resolved instance the host hands a sandboxed helper: the
/// profile's active decision entry with its key filled in. The local
/// mode never sees it; its config and keychain are here.
const VAR_INSTANCE: &str = "RHEMA_DECISION_INSTANCE";

/// The usage text the mode prints on stderr, and the model reads, on
/// an invocation it cannot serve as typed.
const USAGE: &str = "decide: ask a decision model (Jev, Clef, or one OpenRouter routes to) typed questions about a state

    decide noul \"state\" \"yes/no question\"
    decide choice \"state\" \"what to decide\" id[=description]...
    decide score \"state\" \"what to rate\" \"level 0\" \"level 1\" ...
    decide < request.json         the System One request on stdin
    decide -f request.json        the same, from a file

    --json                        print the response, pretty, instead of the lines
    exit 0: answered              exit 1: the provider failed
    exit 2: bad usage             exit 3: the request was refused
    exit 4: not wired in this shell";

/// The most bytes of JSON request one invocation reads from stdin or
/// a file: a state plus questions is text, and an unbounded read of a
/// misdirected pipe is a hang with output.
const MAX_REQUEST_BYTES: u64 = 4 * 1024 * 1024;

/// Wires the session's decide macro: the profile's instance is
/// resolved here, in the host process, and the shell only gets the
/// session endpoint's socket and token. `None` when the profile names
/// no provider or the backend has no route to the host endpoint.
pub(crate) fn wire_for_session(
    session_key: &str,
    profile: &Profile,
    secrets: Option<&dyn SecretStore>,
    sandboxed: Option<&crate::sandbox::Sandboxed>,
    _bash_env: Option<OsString>,
) -> Option<Arc<rhema_bash::decision::Wiring>> {
    let (instance, key) = rhema_decision_registry::selection(Some(profile), secrets)?;
    let endpoint = crate::cap::wire(
        session_key,
        sandboxed,
        crate::cap::Block::Decision(crate::cap::DecisionSetup { instance, key }),
    )?;
    let wiring = rhema_bash::decision::provision_host_call(endpoint.env(), endpoint).ok()?;
    Some(Arc::new(wiring))
}

/// One parsed invocation: the request, and whether to print the raw
/// response instead of the lines. `parse` is the mode's whole argv
/// contract, `run` its whole behavior, so both are testable without a
/// process or a secret store.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Invocation {
    /// The request the call runs.
    pub request: DecisionRequest,
    /// `--json`: the response, pretty, instead of the lines.
    pub json: bool,
}

/// What the argv asked for: a shorthand question, or a JSON request
/// to read from stdin or a file. The mode reads the bytes only in the
/// two `read` arms, so a shorthand never blocks on a stdin nobody
/// writes.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Shape {
    /// A `noul`, `choice`, or `score` verb with its words.
    Shorthand(Invocation),
    /// The System One request on stdin.
    Stdin {
        /// `--json`.
        json: bool,
    },
    /// The System One request in a file `-f` named.
    File {
        /// The path `-f` named.
        path: OsString,
        /// `--json`.
        json: bool,
    },
}

/// Parses the mode's argv (mode token already skipped). Flags come
/// before the verb: `--json` and `-f FILE`. A usage error is
/// [`rhema_bash::decision::EXIT_USAGE`]; the caller prints [`USAGE`].
pub(crate) fn parse(args: &[OsString]) -> Result<Shape, ()> {
    let mut json = false;
    let mut file: Option<OsString> = None;
    let mut words: Vec<String> = Vec::new();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        let text = arg.to_string_lossy();
        match &*text {
            "--json" if words.is_empty() => json = true,
            "-f" | "--file" if words.is_empty() && file.is_none() => {
                let Some(value) = iter.next() else {
                    return Err(());
                };
                file = Some(value.clone());
            }
            // Anything else dash-led is an unknown flag; a lone `-`
            // is a word (a state may be one).
            _ if text.starts_with('-') && text.len() > 1 => return Err(()),
            _ => words.push(text.into_owned()),
        }
    }
    if let Some(path) = file {
        // A file names the whole request; words beside it are a
        // confusion, not a question.
        if !words.is_empty() {
            return Err(());
        }
        return Ok(Shape::File { path, json });
    }
    if words.is_empty() {
        return Ok(Shape::Stdin { json });
    }
    shorthand(&words).map(|request| Shape::Shorthand(Invocation { request, json }))
}

/// The three verbs: `noul STATE INSTRUCTIONS`, `choice STATE
/// INSTRUCTIONS ID[=DESCRIPTION]...`, `score STATE INSTRUCTIONS
/// LEVEL...`. The question's name is the verb's.
fn shorthand(words: &[String]) -> Result<DecisionRequest, ()> {
    let (verb, rest) = words.split_first().ok_or(())?;
    let (state, instructions, criteria) = match rest {
        [state, instructions, criteria @ ..] => (state, instructions, criteria),
        _ => return Err(()),
    };
    let question = match verb.as_str() {
        "noul" if criteria.is_empty() => Question::noul(instructions.as_str()),
        "choice" => {
            let options = criteria
                .iter()
                .map(|word| match word.split_once('=') {
                    Some((id, description)) => (id.to_owned(), description.into()),
                    // A bare id describes itself: the name is the
                    // option, as the model wrote it.
                    None => (word.clone(), word.clone().into()),
                })
                .collect::<Vec<_>>();
            Question::choice(instructions.as_str(), options)
        }
        "score" => Question::score(instructions.as_str(), criteria.iter().map(String::as_str)),
        _ => return Err(()),
    };
    Ok(DecisionRequest::new(state.as_str()).with_question(verb, question))
}

/// Parses a JSON request, the seam's shape with a vendor's `model`
/// key forgiven. The message names the problem.
pub(crate) fn request_from_json(text: &str) -> Result<DecisionRequest, String> {
    // Parse only: a request that parses but does not hold together is
    // refused by the call ([`run`], exit 3), the same as a shorthand
    // that does not, not a usage error.
    DecisionRequest::from_json(text).map_err(|err| format!("not a request: {err}"))
}

/// One invocation's whole answer before it reaches a stream. The
/// modes print it; the host endpoint serializes it.
pub(crate) struct Outcome {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

/// Runs one invocation against a caller-supplied provider and prints
/// the answer: the lines (or the JSON) on stdout, the failure's own
/// text on stderr, the exit codes the usage table names. The seam the
/// CLI tests drive; `cli_run` is this plus the process bits.
///
/// The future runs on its own thread: the binary's `main` is
/// `#[tokio::main]`, so this sync mode can be entered from inside a
/// runtime (the headless path), and a runtime built or blocked on
/// right here would panic with the nested-runtime error. The search
/// mode's arrangement.
pub(crate) fn run(invocation: &Invocation, provider: Arc<dyn DecisionProvider>) -> i32 {
    let outcome = run_outcome(invocation, provider);
    if !outcome.stdout.is_empty() {
        print!("{}", outcome.stdout);
    }
    if !outcome.stderr.is_empty() {
        eprint!("{}", outcome.stderr);
    }
    outcome.code
}

/// [`run`] without the printing: the host endpoint's seam.
pub(crate) fn run_outcome(invocation: &Invocation, provider: Arc<dyn DecisionProvider>) -> Outcome {
    let invocation = invocation.clone();
    let provider_thread = provider.clone();
    let outcome = std::thread::scope(|scope| {
        scope
            .spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("a current-thread runtime cannot fail to build");
                runtime.block_on(rhema_decision::decide(
                    provider_thread.as_ref(),
                    &invocation.request,
                ))
            })
            .join()
            .expect("the decision thread carries no panic of its own")
    });
    match outcome {
        Ok(response) => {
            let text = if invocation.json {
                rhema_decision::render_json(&response)
            } else {
                rhema_decision::render(&response, provider.kind())
            };
            Outcome {
                code: 0,
                stdout: format!("{text}\n"),
                stderr: String::new(),
            }
        }
        Err(DecisionError::Invalid(problem)) => Outcome {
            code: rhema_bash::decision::EXIT_INVALID,
            stdout: String::new(),
            stderr: format!("decide: invalid request: {problem}\n"),
        },
        Err(err) => Outcome {
            code: 1,
            stdout: String::new(),
            stderr: format!("decide: {}: {err}\n", provider.kind()),
        },
    }
}

/// Parses and runs one host endpoint request: the argv the shell
/// sent, plus its stdin, against the instance the host resolved at
/// wiring time.
// Only the Unix host endpoint calls this; the stub has no request
// to run.
#[cfg_attr(not(unix), allow(dead_code))]
pub(crate) fn execute_argv(
    argv: &[String],
    stdin: &str,
    instance: &DecisionInstance,
    key: Option<&str>,
) -> Outcome {
    let args: Vec<OsString> = argv.iter().map(OsString::from).collect();
    let shape = match parse(&args) {
        Ok(shape) => shape,
        Err(()) => {
            return Outcome {
                code: rhema_bash::decision::EXIT_USAGE,
                stdout: String::new(),
                stderr: format!("{USAGE}\n"),
            };
        }
    };
    let invocation = match shape {
        Shape::Shorthand(invocation) => invocation,
        Shape::Stdin { json } => match request_from_json(stdin) {
            Ok(request) => Invocation { json, request },
            Err(problem) => {
                return Outcome {
                    code: rhema_bash::decision::EXIT_USAGE,
                    stdout: String::new(),
                    stderr: format!("decide: {problem}\n"),
                };
            }
        },
        // The shell resolves `-f` by reading the file into stdin; a
        // file path cannot cross to the host's filesystem.
        Shape::File { .. } => {
            return Outcome {
                code: rhema_bash::decision::EXIT_USAGE,
                stdout: String::new(),
                stderr: format!("{USAGE}\n"),
            };
        }
    };
    let provider =
        match rhema_decision_registry::build(instance, key.unwrap_or_default().to_owned()) {
            Ok(provider) => provider,
            Err(err) => {
                return Outcome {
                    code: 1,
                    stdout: String::new(),
                    stderr: format!("decide: {err}\n"),
                };
            }
        };
    run_outcome(&invocation, provider)
}

/// The mode's entry: the process exit code is ours, its stderr is the
/// model's error text, the other modes' contract.
pub fn cli_run(argv: &[OsString]) -> i32 {
    // Parse first: a usage error needs no environment and no config,
    // and it is the one exit code a test can pin without either.
    let shape = match parse(&argv.iter().skip(1).cloned().collect::<Vec<_>>()) {
        Ok(shape) => shape,
        Err(()) => {
            eprintln!("{USAGE}");
            return rhema_bash::decision::EXIT_USAGE;
        }
    };
    let invocation = match shape {
        Shape::Shorthand(invocation) => invocation,
        Shape::Stdin { json } => Invocation {
            json,
            request: match read_request(std::io::stdin().take(MAX_REQUEST_BYTES)) {
                Ok(request) => request,
                Err(problem) => {
                    eprintln!("decide: {problem}");
                    return rhema_bash::decision::EXIT_USAGE;
                }
            },
        },
        Shape::File { path, json } => {
            let read = std::fs::File::open(&path)
                .map(|file| file.take(MAX_REQUEST_BYTES))
                .map_err(|err| format!("{}: {err}", path.display()));
            let request = match read.map(read_request) {
                Ok(Ok(request)) => request,
                Ok(Err(problem)) | Err(problem) => {
                    eprintln!("decide: {problem}");
                    return rhema_bash::decision::EXIT_USAGE;
                }
            };
            Invocation { json, request }
        }
    };
    // The sandboxed helper: the host resolved the instance and handed
    // it over as env. No config file, no keychain, no profile lookup.
    if let Some(instance) = injected_instance() {
        let key = instance.api_key.clone().unwrap_or_default();
        return match rhema_decision_registry::build(&instance, key) {
            Ok(provider) => run(&invocation, provider),
            Err(err) => {
                eprintln!("decide: {err}");
                1
            }
        };
    }
    let Some(profile_name) = std::env::var(rhema_bash::decision::VAR_PROFILE)
        .ok()
        .filter(|name| !name.trim().is_empty())
    else {
        eprintln!(
            "decide: not wired in this shell (no {})",
            rhema_bash::decision::VAR_PROFILE
        );
        return rhema_bash::decision::EXIT_NOT_WIRED;
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
    // The profile's default instance, else the environment tier; the
    // registry's text says which entry or variable falls short when
    // neither builds.
    let provider = rhema_decision_registry::resolve(Some(&profile), secrets.as_deref());
    match provider {
        Ok(provider) => run(&invocation, provider),
        Err(err) => {
            eprintln!("decide: profile \"{profile_name}\": {err}");
            1
        }
    }
}

/// The instance the host handed this process, when
/// [`VAR_INSTANCE`] holds one.
fn injected_instance() -> Option<rhema_config::DecisionInstance> {
    instance_from_injected(&std::env::var(VAR_INSTANCE).ok()?)
}

/// [`injected_instance`] over the raw value, the seam tests drive.
fn instance_from_injected(raw: &str) -> Option<rhema_config::DecisionInstance> {
    serde_json::from_str(raw).ok()
}

/// Reads a request off a reader, bounded, with the same messages the
/// JSON arm gives.
fn read_request(read: impl std::io::Read) -> Result<DecisionRequest, String> {
    let mut text = String::new();
    let mut read = read;
    read.read_to_string(&mut text)
        .map_err(|err| format!("reading the request failed: {err}"))?;
    request_from_json(&text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rhema_decision::mock::MockDecisionProvider;
    use rhema_decision::{Answer, DecisionError};
    use serde_json::json;
    use std::collections::BTreeMap;

    fn os(args: &[&str]) -> Vec<OsString> {
        args.iter().map(OsString::from).collect()
    }

    fn answers() -> BTreeMap<String, Answer> {
        BTreeMap::from([("noul".to_owned(), Answer::Noul { noul: 0.9 })])
    }

    #[test]
    fn a_noul_is_two_words_after_the_verb() {
        let Shape::Shorthand(invocation) =
            parse(&os(&["noul", "the order never came", "Wants a person?"])).unwrap()
        else {
            panic!("expected a shorthand");
        };
        assert_eq!(invocation.request.state, json!("the order never came"));
        assert_eq!(
            invocation.request.questions["noul"],
            Question::noul("Wants a person?")
        );
        assert!(!invocation.json);
        assert!(parse(&os(&["noul", "state"])).is_err());
        assert!(parse(&os(&["noul", "state", "q", "extra"])).is_err());
    }

    #[test]
    fn a_choice_takes_ids_with_optional_descriptions() {
        let Shape::Shorthand(invocation) = parse(&os(&[
            "choice",
            "state",
            "Which team",
            "billing=Payments and refunds",
            "technical",
        ]))
        .unwrap() else {
            panic!("expected a shorthand");
        };
        assert_eq!(
            invocation.request.questions["choice"],
            Question::choice(
                "Which team",
                [
                    ("billing", json!("Payments and refunds")),
                    ("technical", json!("technical"))
                ]
            )
        );
        assert!(invocation.request.validate().is_ok());
        // One option is not a choice.
        let Shape::Shorthand(one) = parse(&os(&["choice", "s", "q", "a"])).unwrap() else {
            panic!("expected a shorthand");
        };
        assert!(one.request.validate().is_err());
    }

    #[test]
    fn a_score_takes_its_levels_in_order() {
        let Shape::Shorthand(invocation) = parse(&os(&[
            "score",
            "state",
            "Impact",
            "None",
            "One order",
            "All orders",
        ]))
        .unwrap() else {
            panic!("expected a shorthand");
        };
        assert_eq!(
            invocation.request.questions["score"],
            Question::score("Impact", ["None", "One order", "All orders"])
        );
    }

    #[test]
    fn no_words_is_stdin_and_a_file_names_the_request() {
        assert_eq!(parse(&os(&[])), Ok(Shape::Stdin { json: false }));
        assert_eq!(parse(&os(&["--json"])), Ok(Shape::Stdin { json: true }));
        assert_eq!(
            parse(&os(&["-f", "req.json", "--json"])),
            Ok(Shape::File {
                path: OsString::from("req.json"),
                json: true
            })
        );
        // A flag naming another instance is not the model's to give:
        // the profile picks, and the flag is usage.
        assert!(parse(&os(&["--instance", "jev", "noul", "s", "q?"])).is_err());
        // A file and a verb together is a confusion.
        assert!(parse(&os(&["-f", "req.json", "noul", "s", "q"])).is_err());
        assert!(parse(&os(&["-f"])).is_err(), "a file flag with no path");
    }

    #[test]
    fn flags_after_the_verb_are_words_and_unknown_flags_are_usage() {
        // `--json` after the verb is the state's first word, so the
        // verb must come first; an unknown flag before is usage.
        assert!(parse(&os(&["--deep", "noul", "s", "q"])).is_err());
        assert!(parse(&os(&["-x", "noul", "s", "q"])).is_err());
        assert!(parse(&os(&["shrug", "s", "q"])).is_err(), "no such verb");
    }

    #[test]
    fn json_requests_parse_and_forgive_a_model_key() {
        let request = request_from_json(
            r#"{"model": "jev-latest", "state": "s",
                "questions": {"q": {"type": "noul", "instructions": "x"}}}"#,
        )
        .unwrap();
        assert_eq!(request.questions.len(), 1);
        assert!(request.validate().is_ok());
        let err = request_from_json("not json").unwrap_err();
        assert!(err.contains("not a request"), "{err}");
        // Parses, and does not hold together: the call refuses it.
        let request = request_from_json(r#"{"state": "s", "questions": {}}"#).unwrap();
        assert!(
            request
                .validate()
                .unwrap_err()
                .contains("at least one question")
        );
        let request = request_from_json(
            r#"{"state": "s", "questions": {"q": {"type": "choice", "instructions": "x", "criteria": {"a": "A"}}}}"#,
        )
        .unwrap();
        assert!(request.validate().unwrap_err().contains("question \"q\""));
    }

    #[test]
    fn the_run_prints_the_lines_or_the_json_and_fails_with_the_codes() {
        let winner = MockDecisionProvider::new("jev").shared();
        // Two scripted answers: the lines run and the json run.
        winner.push_ok(rhema_decision::DecisionResponse::new(
            "jev-1.13.0",
            answers(),
        ));
        winner.push_ok(rhema_decision::DecisionResponse::new(
            "jev-1.13.0",
            answers(),
        ));
        let winner: Arc<dyn DecisionProvider> = winner;
        let Shape::Shorthand(invocation) = parse(&os(&["noul", "state", "q"])).unwrap() else {
            panic!("expected a shorthand");
        };
        assert_eq!(run(&invocation, winner.clone()), 0);
        let Shape::Shorthand(verbatim) = parse(&os(&["--json", "noul", "state", "q"])).unwrap()
        else {
            panic!("expected a shorthand");
        };
        assert!(verbatim.json);
        assert_eq!(run(&verbatim, winner), 0);

        let loser = MockDecisionProvider::new("clef").shared();
        loser.push_err(DecisionError::Auth);
        let loser: Arc<dyn DecisionProvider> = loser;
        assert_eq!(run(&invocation, loser), 1);

        // A request refused as invalid is the request's fault, its
        // own exit code, and never reaches the vendor: a shorthand
        // and a JSON request that parses alike.
        let untouched = MockDecisionProvider::new("jev").shared();
        let provider: Arc<dyn DecisionProvider> = untouched.clone();
        let Shape::Shorthand(one_option) = parse(&os(&["choice", "s", "q", "a"])).unwrap() else {
            panic!("expected a shorthand");
        };
        assert_eq!(
            run(&one_option, provider.clone()),
            rhema_bash::decision::EXIT_INVALID
        );
        let request = read_request(&br#"{"state": "s", "questions": {}}"#[..]).unwrap();
        assert_eq!(
            run(
                &Invocation {
                    json: false,
                    request
                },
                provider
            ),
            rhema_bash::decision::EXIT_INVALID
        );
        assert_eq!(untouched.calls(), 0);
    }

    #[test]
    fn the_wiring_gate_closes_on_sandbox_and_on_no_provider() {
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
    fn the_profiles_off_switch_closes_the_wiring() {
        let mut profile = Profile::default();
        let section = profile
            .decision
            .get_or_insert_with(rhema_config::DecisionSettings::default);
        section.instances.insert(
            "Jev".into(),
            rhema_config::DecisionInstance {
                provider: "jev".into(),
                api_key: Some("k".into()),
                secret_ref: None,
                model: None,
                account_id: None,
                base_url: None,
            },
        );
        profile.decision_enabled = Some(false);
        assert!(
            wire_for_session("personal", &profile, None, None, None).is_none(),
            "a stored instance and its key stay out while the switch is off"
        );
    }

    #[tokio::test]
    async fn a_run_inside_a_runtime_does_not_panic() {
        // The binary's main is #[tokio::main], and the headless path
        // enters this mode from inside that runtime; the worker-thread
        // escape survives the same context.
        let winner = MockDecisionProvider::new("jev").shared();
        winner.push_ok(rhema_decision::DecisionResponse::new(
            "jev-1.13.0",
            answers(),
        ));
        let winner: Arc<dyn DecisionProvider> = winner;
        let Shape::Shorthand(invocation) = parse(&os(&["noul", "state", "q"])).unwrap() else {
            panic!("expected a shorthand");
        };
        let code = tokio::task::spawn_blocking(move || run(&invocation, winner))
            .await
            .unwrap();
        assert_eq!(code, 0);
    }

    #[test]
    fn a_request_reads_off_a_reader_with_the_json_arms_messages() {
        let good = r#"{"state": "s", "questions": {"q": {"type": "noul", "instructions": "x"}}}"#;
        assert!(read_request(good.as_bytes()).is_ok());
        assert!(read_request("garbage".as_bytes()).is_err());
    }
}

#[cfg(test)]
mod sandbox_tests {
    use super::*;
    use crate::args::SandboxChoice;
    use crate::sandbox::Sandboxed;
    use rhema_bash::ShellTools;
    use rhema_config::{DecisionInstance, DecisionSettings, SandboxSettings};
    use std::collections::BTreeMap;
    use std::sync::Arc;

    fn sandboxed() -> (Sandboxed, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let sandboxed = Sandboxed {
            choice: SandboxChoice::Docker,
            target: "test".to_owned(),
            tools: Arc::new(ShellTools::new("/workspace")),
            workspace: "/workspace".to_owned(),
            skills: Vec::new(),
            instance_lock: None,
            managed: None,
            shared: Some(rhema_sandbox::kit::SharedRoot::new(dir.path())),
            mcp: crate::mcp_config::McpStore::local(None, &std::collections::BTreeSet::new()),
            session: Default::default(),
            sandbox: None,
            remote: None,
            remote_session: Default::default(),
        };
        (sandboxed, dir)
    }

    fn profile(inject: bool) -> Profile {
        Profile {
            sandbox: Some(SandboxSettings {
                inject_credentials: inject,
                ..SandboxSettings::default()
            }),
            decision: Some(DecisionSettings {
                instance: None,
                instances: BTreeMap::from([(
                    "local".to_owned(),
                    DecisionInstance {
                        provider: "jev".into(),
                        api_key: Some("literal-key".into()),
                        model: Some("m".into()),
                        base_url: Some("http://box".into()),
                        ..DecisionInstance::default()
                    },
                )]),
            }),
            ..Profile::default()
        }
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
    fn a_remote_sandbox_with_no_forward_path_does_not_wire() {
        let sandboxed = remote_sandbox();
        assert!(
            wire_for_session(
                "sandbox-decision-ssh-no-forward",
                &profile(true),
                None,
                Some(&sandboxed),
                None,
            )
            .is_none()
        );
    }

    #[test]
    fn a_mounted_sandbox_wires_the_endpoint_and_never_the_key() {
        let (sandboxed, _dir) = sandboxed();
        let wiring = wire_for_session(
            "sandbox-decision-endpoint",
            &profile(true),
            None,
            Some(&sandboxed),
            None,
        )
        .expect("the mounted session wires");
        let socket = wiring
            .shell_env
            .get(rhema_bash::runtime::VAR_CAP_SOCKET)
            .expect("the cap socket");
        assert!(socket.ends_with("cap.sock"), "{socket}");
        assert!(
            wiring
                .shell_env
                .get(rhema_bash::runtime::VAR_CAP_TOKEN)
                .is_some_and(|token| token.len() == 64)
        );
        assert!(
            wiring
                .shell_env
                .values()
                .all(|value| !value.contains("literal-key"))
        );
    }
}

#[cfg(test)]
mod injected_tests {
    use super::instance_from_injected;

    #[test]
    fn a_host_resolved_instance_parses() {
        let raw = r#"{"provider":"jev","api_key":"k","model":"m","base_url":"http://box"}"#;
        let instance = instance_from_injected(raw).expect("the host's shape parses");
        assert_eq!(instance.provider, "jev");
        assert_eq!(instance.api_key.as_deref(), Some("k"));
        assert_eq!(instance.base_url.as_deref(), Some("http://box"));
    }

    #[test]
    fn a_malformed_instance_falls_through() {
        assert!(instance_from_injected("not json").is_none());
    }
}
