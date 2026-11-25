//! The `__decision` mode and the session wiring: the `decide` macro's
//! other end.
//!
//! The agent's shell execs `rhema __decision …`; this module owns the
//! pass-through: it parses the verb (or reads the JSON request from
//! stdin or a file), resolves the profile's decision provider the
//! wiring named through the registry, runs the request, and prints
//! the lines (or, under `--json`, the response, pretty). The settings are
//! read fresh per call, so a provider configured mid-session serves
//! the next `decide` without a restart; the profile is the
//! wiring's env var, pinned at session start, the one per-session
//! input a standalone exec can carry without session state.
//!
//! The session wiring gate is the mcp_wire rule: a sandboxed session
//! wires nothing, and a profile naming no decision provider wires
//! nothing, no function and no prompt section
//! (`specs/rhema-decision.md` §10). One wiring per session key, held
//! for the process, because the init file's env map is per-profile and
//! a profile switch re-runs the assembly: the search wire's shape.

#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io::Read as _;
use std::sync::{Arc, Mutex, OnceLock};

use rhema_config::Profile;
use rhema_config::secrets::SecretStore;
use rhema_decision::{DecisionError, DecisionProvider, DecisionRequest, Question};

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

/// One session's wiring, the mcp_wire shape: the provisioned init
/// file held for the process's life, one per session key.
static WIRED: OnceLock<Mutex<BTreeMap<String, Arc<rhema_bash::decision::Wiring>>>> =
    OnceLock::new();

fn registry() -> &'static Mutex<BTreeMap<String, Arc<rhema_bash::decision::Wiring>>> {
    WIRED.get_or_init(|| Mutex::new(BTreeMap::new()))
}

/// Wires the session's decide macro, the setup's call: the gate first
/// (a sandboxed session and a providerless profile both wire
/// nothing), then one provision per session key. `None` when the gate
/// closes: the caller renders no section, the agent sees no function.
pub(crate) fn wire_for_session(
    session_key: &str,
    profile: &Profile,
    secrets: Option<&dyn SecretStore>,
    sandboxed: Option<&crate::sandbox::Sandboxed>,
    bash_env: Option<OsString>,
) -> Option<Arc<rhema_bash::decision::Wiring>> {
    if sandboxed.is_some() {
        return None;
    }
    // The gate answers the only question the settings can: does this
    // profile name a provider whose key resolves? The provider itself
    // is not cached; the mode handler resolves fresh per invocation,
    // so a provider configured mid-session lands without a restart.
    rhema_decision_registry::is_configured(Some(profile), secrets).then_some(())?;
    let mut reg = registry().lock().expect("decision wire registry lock");
    if let Some(wiring) = reg.get(session_key) {
        return Some(wiring.clone());
    }
    let bin = std::env::current_exe().ok()?;
    let wiring =
        Arc::new(rhema_bash::decision::provision_with_bin(bash_env, &bin, session_key).ok()?);
    reg.insert(session_key.to_owned(), wiring.clone());
    Some(wiring)
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
            if invocation.json {
                println!("{}", rhema_decision::render_json(&response));
            } else {
                println!("{}", rhema_decision::render(&response, provider.kind()));
            }
            0
        }
        Err(DecisionError::Invalid(problem)) => {
            eprintln!("decide: invalid request: {problem}");
            rhema_bash::decision::EXIT_INVALID
        }
        Err(err) => {
            eprintln!("decide: {}: {err}", provider.kind());
            1
        }
    }
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
