//! Session setup, one-shot mode, and the interactive REPL.

use std::io::{BufRead, Write};
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, bail};
use rhema_coding_agent::{AllowAll, Approval, CodingAgent, CodingAgentConfig};
use rhema_core::{ModelProvider, Session};
use rhema_provider_google::GoogleProvider;
use rhema_provider_llama_cpp::LlamaCppProvider;

use crate::approval::ConsoleApproval;
use crate::args::{Args, ProviderChoice};
use crate::sink::{TermSink, format_usage};

/// The REPL prompt, on stderr so stdout stays clean for streamed text.
const PROMPT: &str = "› ";

/// The REPL's slash commands, one line each.
const HELP: &str = "commands:\n  /help   show this help\n  /clear  start a fresh session (drop history)\n  /exit   leave the REPL\n  /quit   leave the REPL\nanything else is sent to the model as a prompt.";

/// What one REPL input line means.
#[derive(Debug, PartialEq, Eq)]
pub enum Input {
    /// Blank line: re-prompt, send nothing.
    Empty,
    /// Text to send to the model.
    Prompt(String),
    /// A recognized slash command.
    Command(SlashCommand),
    /// A `/`-prefixed line that names no known command.
    Unknown,
}

/// The slash commands the REPL understands.
#[derive(Debug, PartialEq, Eq)]
pub enum SlashCommand {
    /// List the commands.
    Help,
    /// Leave the REPL.
    Exit,
    /// Drop the session history.
    Clear,
}

/// Classifies one raw input line (terminating newline optional).
pub fn classify(line: &str) -> Input {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return Input::Empty;
    }
    let Some(command) = trimmed.strip_prefix('/') else {
        return Input::Prompt(trimmed.to_owned());
    };
    match command {
        "help" => Input::Command(SlashCommand::Help),
        "exit" | "quit" => Input::Command(SlashCommand::Exit),
        "clear" => Input::Command(SlashCommand::Clear),
        _ => Input::Unknown,
    }
}

/// The model string sent to a llama.cpp server when none is set. The
/// server typically ignores it, but OpenAI-shaped APIs require the field.
pub const DEFAULT_LLAMA_MODEL: &str = "local";

/// The model used when neither `--model` nor the stored config names one.
/// Google closed `gemini-2.5-flash` to new users; the error's own
/// recommendation is the default here.
pub const DEFAULT_MODEL: &str = "gemini-3.8-flash";

/// Resolves the API key: `GEMINI_API_KEY`, then `GOOGLE_API_KEY`, then the
/// key stored by the TUI login in the rhema config. The key is never
/// accepted as a CLI flag — shells keep history.
pub fn resolve_api_key() -> anyhow::Result<String> {
    let from = |name: &str| std::env::var(name).ok();
    let stored = stored_config().google_api_key;
    pick_key(from("GEMINI_API_KEY"), from("GOOGLE_API_KEY"), stored)
}

/// Loads the rhema config; a broken file warns and behaves as empty rather
/// than bricking the CLI.
fn stored_config() -> rhema_config::Config {
    match rhema_config::Config::load() {
        Ok(config) => config,
        Err(err) => {
            eprintln!("warning: ignoring unreadable rhema config: {err}");
            rhema_config::Config::default()
        }
    }
}

/// Pure core of [`resolve_api_key`], so the fallback order is testable
/// without mutating process state. A whitespace-only value counts as
/// missing.
fn pick_key(
    gemini: Option<String>,
    google: Option<String>,
    stored: Option<String>,
) -> anyhow::Result<String> {
    gemini
        .or(google)
        .or(stored)
        .map(|key| key.trim().to_owned())
        .filter(|key| !key.is_empty())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "missing API key: set GEMINI_API_KEY (or GOOGLE_API_KEY), or run rhema-tui once to log in"
            )
        })
}

/// Runs the CLI: validates options, builds the provider and agent, then
/// either executes the one-shot prompt or enters the REPL.
pub async fn run(args: Args) -> anyhow::Result<()> {
    let config = stored_config();
    let provider_choice = args
        .provider
        .unwrap_or_else(|| ProviderChoice::from_stored(config.provider.as_deref()));
    // The key check precedes any session work: no point touching the
    // filesystem for a run that cannot authenticate. The llama.cpp server
    // needs no key.
    let api_key = match provider_choice {
        ProviderChoice::Google => resolve_api_key()?,
        ProviderChoice::LlamaCpp => String::new(),
    };

    if let (Some(resume), Some(save)) = (&args.resume, &args.save) {
        if resume != save {
            bail!(
                "--resume {:?} and --save {:?} differ; when resuming and saving, they must name the same file",
                resume,
                save
            );
        }
    }

    let provider: Arc<dyn ModelProvider> = match provider_choice {
        ProviderChoice::LlamaCpp => {
            let mut llamacpp = LlamaCppProvider::new(
                args.llama_url
                    .clone()
                    .unwrap_or_else(|| config.llama_cpp_url()),
            );
            if let Some(key) = config.llama_cpp_api_key() {
                llamacpp = llamacpp.with_api_key(key);
            }
            Arc::new(llamacpp)
        }
        ProviderChoice::Google => Arc::new(GoogleProvider::new(api_key)),
    };
    let model = args.model.clone().unwrap_or_else(|| {
        config
            .model
            .clone()
            .unwrap_or_else(|| match provider_choice {
                ProviderChoice::LlamaCpp => DEFAULT_LLAMA_MODEL.to_owned(),
                ProviderChoice::Google => DEFAULT_MODEL.to_owned(),
            })
    });
    let config = CodingAgentConfig {
        max_turn_steps: args.max_steps,
        ..CodingAgentConfig::new(model, &args.workspace)
    };
    let session = match &args.resume {
        Some(path) => CodingAgent::load_session_from(path)
            .with_context(|| format!("cannot resume session: {}", path.display()))?,
        None => Session::default(),
    };
    let mut agent = CodingAgent::from_session(provider, config, session)?;
    let approval: Arc<dyn Approval> = if args.no_confirm {
        Arc::new(AllowAll)
    } else {
        Arc::new(ConsoleApproval)
    };
    let mut sink = TermSink::new();

    match args.prompt {
        Some(prompt) => {
            let output = execute_turn(
                &mut agent,
                &prompt,
                &mut sink,
                &approval,
                args.save.as_deref(),
            )
            .await?;
            eprintln!("{}", format_usage(sink.steps(), output.usage));
        }
        None => repl(&mut agent, &mut sink, &approval, args.save.as_deref()).await?,
    }
    Ok(())
}

/// The interactive loop. Errors from a turn are printed and swallowed: a
/// failed turn must not kill the session.
async fn repl(
    agent: &mut CodingAgent,
    sink: &mut TermSink,
    approval: &Arc<dyn Approval>,
    save: Option<&Path>,
) -> anyhow::Result<()> {
    loop {
        eprint!("{PROMPT}");
        let _ = std::io::stderr().flush();

        let mut line = String::new();
        // Blocks the runtime worker while waiting, which is fine: nothing
        // else in the process is concurrent with the REPL.
        if std::io::stdin().lock().read_line(&mut line)? == 0 {
            return Ok(()); // EOF (Ctrl-D): exit cleanly.
        }

        match classify(&line) {
            Input::Empty => {}
            Input::Command(SlashCommand::Help) => eprint!("{HELP}"),
            Input::Command(SlashCommand::Exit) => return Ok(()),
            Input::Command(SlashCommand::Clear) => {
                *agent.session_mut() = Session::default();
                eprintln!("session cleared");
                // Keep the checkpoint file consistent with the fresh
                // history; ignored when --save was not given.
                if let Some(path) = save {
                    agent
                        .save_session_to(&path)
                        .with_context(|| format!("cannot save session: {}", path.display()))?;
                }
            }
            Input::Unknown => eprintln!("unknown command, /help"),
            Input::Prompt(text) => match execute_turn(agent, &text, sink, approval, save).await {
                Ok(output) => eprintln!("{}", format_usage(sink.steps(), output.usage)),
                // One clean line; the session survives for the next prompt.
                Err(err) => eprintln!("error: {err:#}"),
            },
        }
    }
}

/// One turn against the agent, checkpointing afterwards when `--save` is
/// set (crash-safe: the file always reflects every completed turn).
async fn execute_turn(
    agent: &mut CodingAgent,
    input: &str,
    sink: &mut TermSink,
    approval: &Arc<dyn Approval>,
    save: Option<&Path>,
) -> anyhow::Result<rhema_core::ModelOutput> {
    sink.begin_turn();
    let output = agent.turn(input, sink, approval.clone()).await?;
    if let Some(path) = save {
        agent
            .save_session_to(&path)
            .with_context(|| format!("cannot save session: {}", path.display()))?;
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blank_lines_reprompt() {
        assert_eq!(classify(""), Input::Empty);
        assert_eq!(classify("   \n"), Input::Empty);
    }

    #[test]
    fn slash_commands_dispatch() {
        assert_eq!(classify("/help\n"), Input::Command(SlashCommand::Help));
        assert_eq!(classify("/exit"), Input::Command(SlashCommand::Exit));
        assert_eq!(classify("/quit"), Input::Command(SlashCommand::Exit));
        assert_eq!(classify("/clear"), Input::Command(SlashCommand::Clear));
    }

    #[test]
    fn unknown_slash_lines_are_rejected_not_sent() {
        assert_eq!(classify("/foo"), Input::Unknown);
        assert_eq!(classify("/"), Input::Unknown);
        assert_eq!(classify("/help extra"), Input::Unknown);
    }

    #[test]
    fn plain_lines_become_prompts_with_whitespace_trimmed() {
        assert_eq!(
            classify("fix the test\n"),
            Input::Prompt("fix the test".into())
        );
        assert_eq!(classify("  hello  "), Input::Prompt("hello".into()));
    }

    #[test]
    fn the_key_prefers_gemini_then_google_then_stored_then_errors() {
        assert_eq!(pick_key(Some("a".into()), None, None).unwrap(), "a");
        assert_eq!(pick_key(None, Some("b".into()), None).unwrap(), "b");
        assert_eq!(pick_key(None, None, Some("c".into())).unwrap(), "c");
        assert_eq!(
            pick_key(Some("a".into()), Some("b".into()), Some("c".into())).unwrap(),
            "a"
        );
        let err = pick_key(None, None, None).unwrap_err().to_string();
        assert!(err.contains("GEMINI_API_KEY"), "{err}");
        // A whitespace-only value does not count as present.
        let err = pick_key(Some("  ".into()), None, None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("GEMINI_API_KEY"), "{err}");
    }

    #[test]
    fn the_help_text_documents_every_command() {
        for command in ["/help", "/clear", "/exit", "/quit"] {
            assert!(HELP.contains(command), "help missing {command}");
        }
    }
}
