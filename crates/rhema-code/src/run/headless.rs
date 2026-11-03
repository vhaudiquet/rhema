//! The one-shot runner: `rhema run` turns one prompt with no
//! terminal — the sink that maps model events to stdout/stderr, the
//! provider default models, and the checkpoint a failed turn leaves
//! for `--resume`.

use super::*;

/// The model used headless when neither `--model` nor the stored config
/// names one.
const DEFAULT_MODEL: &str = "gemini-3.8-flash";
/// The model string sent to a llama.cpp server when none is set. The
/// server typically ignores it, but OpenAI-shaped APIs require the field.
const DEFAULT_LLAMA_MODEL: &str = "local";
/// The headless Anthropic model when neither `--model` nor the stored
/// config names one: the mid-tier Claude the adapter's own docs use as
/// its example id. This fallback is a safety net, not the normal path —
/// the login and picker flow always stores a model — so it only has to
/// be an id the API keeps answering for.
const DEFAULT_ANTHROPIC_MODEL: &str = "claude-sonnet-4-5";
/// The headless OpenAI model when neither `--model` nor the stored
/// config names one, chosen with the same reasoning as
/// [`DEFAULT_ANTHROPIC_MODEL`]: the GPT-5 family id, current at the time
/// of writing.
const DEFAULT_OPENAI_MODEL: &str = "gpt-5";
/// The headless DeepSeek model when neither `--model` nor the stored
/// config names one: the non-thinking chat model — `deepseek-reasoner`
/// thinks by default and is the slower choice.
const DEFAULT_DEEPSEEK_MODEL: &str = "deepseek-chat";
/// The headless Z.ai Coding Plan model when neither `--model` nor the
/// stored config names one, chosen with the same reasoning as
/// [`DEFAULT_ANTHROPIC_MODEL`]: the plan's flagship id, as Z.ai's own
/// client spells it.
const DEFAULT_ZAI_MODEL: &str = "GLM-5.3";
/// The headless Copilot model when neither `--model` nor the stored
/// config names one, chosen with the same reasoning as
/// [`DEFAULT_ANTHROPIC_MODEL`]: the GPT-4.1 id, the proxy's long-running
/// default workhorse.
const DEFAULT_COPILOT_MODEL: &str = "gpt-4.1";

/// The headless event sink: reply text streams raw to stdout (so pipes
/// capture exactly the reply), tool traffic goes to stderr, and the
/// program-status record follows the turn.
pub(crate) struct HeadlessSink {
    /// The intent the status shows: the prompt's first line until a
    /// tool call names a more specific one.
    msg: Option<String>,
    /// The last announced step, for the status' `progress`.
    step: Option<u32>,
    /// The turn's step cap, the denominator of `progress`.
    max_steps: u32,
}

impl HeadlessSink {
    /// Re-announces the working state from the current step and tool.
    fn report(&self) {
        status::emit(&Status::Working {
            msg: self.msg.clone(),
            progress: self.step.map(|step| status::progress(step, self.max_steps)),
        });
    }
}

impl EventSink for HeadlessSink {
    fn emit(&mut self, event: ModelEvent<'_>) {
        use std::io::Write;
        match event {
            ModelEvent::TextDelta(delta) => {
                print!("{delta}");
                let _ = std::io::stdout().flush();
            }
            ModelEvent::StepStart { iteration } => {
                self.step = Some(iteration);
                self.report();
            }
            ModelEvent::ToolCallStart { name, .. } => {
                eprintln!("⌁ {name}");
                self.msg = Some(name.to_owned());
                self.report();
            }
            ModelEvent::ToolResult {
                content, is_error, ..
            } => {
                let mark = if is_error { "✗" } else { "✓" };
                eprintln!("  {mark} {}", first_line(content));
            }
            _ => {}
        }
    }
}

/// The whole of stdin, as the `run -` prompt form and a piped stdin
/// read it.
pub(crate) fn read_prompt_from_stdin() -> anyhow::Result<String> {
    use std::io::Read;
    let mut text = String::new();
    std::io::stdin().read_to_string(&mut text)?;
    Ok(text)
}

/// The headless one-shot: resolve the backend, run one turn, print the
/// reply to stdout and everything else to stderr. Built for pipes.
///
/// The program-status record covers the whole run — credential
/// resolution included — and is what survives it: `done` or `error`,
/// never cleared (see `crate::status`). A usage failure (no prompt)
/// exits before any record exists.
pub(crate) async fn run_headless(args: RunArgs) -> anyhow::Result<()> {
    let prompt = match args.prompt.as_deref() {
        Some("-") => read_prompt_from_stdin()?,
        Some(text) => text.to_owned(),
        // A piped stdin is a prompt source, not a REPL: with a terminal
        // stdin and no prompt there is nothing to run.
        None if !std::io::stdin().is_terminal() => read_prompt_from_stdin()?,
        None => anyhow::bail!("no prompt: pass a prompt or pipe one on stdin"),
    };
    if prompt.trim().is_empty() {
        anyhow::bail!("empty prompt");
    }
    status::emit(&Status::Working {
        msg: Some(first_line(&prompt)),
        progress: None,
    });
    let outcome = run_prompt(args, prompt).await;
    match outcome {
        Ok(()) => status::emit(&Status::Done),
        Err(ref err) => status::emit(&Status::Error {
            msg: Some(first_line(&err.to_string())),
        }),
    }
    outcome
}

/// The body of the one-shot: everything from the stored config to the
/// finished turn.
pub(crate) async fn run_prompt(args: RunArgs, prompt: String) -> anyhow::Result<()> {
    let mut config = match Config::load() {
        Ok(config) => config,
        Err(err) => {
            eprintln!("warning: ignoring unreadable rhema config: {err}");
            Config::default()
        }
    };
    migrate_stored_keys(&mut config);
    let secrets = KeyringSecretStore::new();
    // Same profile resolution as the TUI: an explicit --profile is
    // honored as named, a directory with a remembered profile reopens on
    // it, an absent one starts on a profile that can serve turns. No
    // login screen exists headless: when nothing is usable the backend
    // errors below, naming the fix.
    let workspace =
        std::fs::canonicalize(&args.workspace).unwrap_or_else(|_| args.workspace.clone());
    let profile = args
        .profile
        .clone()
        .unwrap_or_else(|| startup_profile(&config, &workspace, Some(&secrets)));
    let effective = config.effective(Some(&profile));
    // RHEMA_PROVIDER (inside provider_name) wins over the stored
    // profile, the same env-wins convention the keys follow. Nothing
    // configured is an error naming the fixes: headless has no login
    // screen to fall through to.
    let provider_choice = ProviderChoice::from_stored(effective.provider_name().as_deref())
        .ok_or_else(|| anyhow::anyhow!(crate::app::NO_PROVIDER_HELP))?;
    // The same resolution the TUI's startup runs; a failure names the
    // fix, because headless has no login screen to fall through to.
    let backend = Backend::from_profile_or_help(&effective, Some(&secrets))
        .map_err(|message| anyhow::anyhow!("{message}"))?;
    let model = args.model.clone().unwrap_or_else(|| {
        effective
            .model
            .clone()
            .unwrap_or_else(|| match provider_choice {
                ProviderChoice::LlamaCpp => DEFAULT_LLAMA_MODEL.to_owned(),
                ProviderChoice::Anthropic => DEFAULT_ANTHROPIC_MODEL.to_owned(),
                ProviderChoice::OpenAi => DEFAULT_OPENAI_MODEL.to_owned(),
                ProviderChoice::DeepSeek => DEFAULT_DEEPSEEK_MODEL.to_owned(),
                ProviderChoice::Zai => DEFAULT_ZAI_MODEL.to_owned(),
                ProviderChoice::Copilot => DEFAULT_COPILOT_MODEL.to_owned(),
                ProviderChoice::Google | ProviderChoice::OpenRouter => DEFAULT_MODEL.to_owned(),
            })
    });

    let thinking = args
        .thinking
        .map(rhema_core::request::ThinkingEffort::from)
        .unwrap_or_else(|| effective.thinking_effort());
    let base_system = resolve_base_system(Config::system_prompt_path().ok().as_deref());
    let mut agent_config = rhema_coding_agent::CodingAgentConfig {
        max_turn_steps: args.max_steps,
        transcript: (!args.no_transcript).then(rhema_transcript::TranscriptSettings::default),
        thinking,
        base_system: base_system.clone(),
        disabled_skills: config.skills_disabled.clone(),
        ..rhema_coding_agent::CodingAgentConfig::new(&model, &workspace)
    };
    // Where bash runs: the sandbox choice engages only off `local`, and
    // before the subagent wire below — their inner shell inherits the
    // executor installed here (ShellTools::executor()), while their
    // definition discovery stays host-side, so the sandbox-side
    // workspace string lands on the config after the wire.
    let choice = crate::sandbox::resolve_choice(&effective);
    let sandboxed = match choice {
        crate::args::SandboxChoice::Local => None,
        non_local => Some(
            crate::sandbox::assemble(
                non_local,
                &effective,
                &workspace,
                Some(&secrets),
                base_system.as_deref(),
            )
            .await?,
        ),
    };
    if let Some(assembled) = &sandboxed {
        assembled.apply_tools(&mut agent_config);
    }
    // Subagents ride in as one tool: the built-in general-purpose
    // definition plus whatever the workspace defines.
    // The handle rides to the UI in the wire-up piece; the registry
    // is live either way.
    let (mut agent_config, _subagents) = rhema_subagents::wire(backend.provider(), agent_config);
    if let Some(assembled) = sandboxed.as_ref() {
        assembled.apply_workspace(&mut agent_config);
    }
    // The workspace remembers the profile that served it, the same
    // memory the next launch here reads back. Best effort: a failed
    // write warns without failing the turn.
    if config.last_profile_in(&workspace).as_deref() != Some(profile.as_str()) {
        config.note_profile_in(&workspace, &profile);
        if let Err(err) = config.save() {
            eprintln!("warning: the profile choice was not remembered: {err}");
        }
    }
    let session = match &args.resume {
        Some(path) => CodingAgent::load_session_from(path)
            .with_context(|| format!("cannot resume session: {}", path.display()))?,
        None => Session::default(),
    };
    let mut agent = CodingAgent::from_session(backend.provider(), agent_config, session)?;
    // Auto-approved for now: the Approval seam is where command
    // allowlists will land.
    let approval: Arc<dyn Approval> = Arc::new(rhema_coding_agent::AllowAll);

    // The checkpoint, opt-in headless: when `--save` names a file, the
    // engine hands the session over at every stream boundary — before
    // each request, after the assistant message lands and before the
    // first tool runs, after each step's results — so a crash or a kill
    // mid-round, mid-stream included, leaves a resumable file there,
    // which the next `--resume`'s revive closes, instead of no record at
    // all.
    let save = args.save.clone();
    if let Some(dir) = save.as_ref().and_then(|path| path.parent()) {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("cannot create the checkpoint directory {}", dir.display()))?;
    }
    if let Some(path) = &save {
        let hook_path = path.clone();
        agent.set_commit_hook(Some(Box::new(move |session: &Session| {
            crate::sessions::write_session(&hook_path, session)
                .map_err(|err| rhema_core::CommitError(err.to_string()))
        })));
    }

    let mut sink = HeadlessSink {
        msg: Some(first_line(&prompt)),
        step: None,
        max_steps: args.max_steps,
    };
    let outcome = agent.turn(prompt, &mut sink, approval).await;
    let served: anyhow::Result<()> = match outcome {
        Ok(output) => {
            // Checkpoint after the completed turn: the file's freshest
            // complete state — the stream commits already brought it
            // here, this re-writes it with the final usage attached.
            if let Some(path) = &save
                && let Err(err) = agent.save_session_to(path)
            {
                eprintln!("warning: the session was not checkpointed: {err}");
            }
            eprintln!(
                "· {} · ctx {}",
                model,
                crate::screens::format_tokens(Some(crate::app::context_tokens(&output.usage)))
            );
            // The workspace remembers the sandbox kind that just served it —
            // the twin of the profile memory above. Best effort, the same
            // style: a failed write warns without failing the run.
            if config.last_sandbox_in(&workspace).as_deref() != Some(choice.stored_name()) {
                config.note_sandbox_in(&workspace, choice.stored_name());
                if let Err(err) = config.save() {
                    eprintln!("warning: the sandbox choice was not remembered: {err}");
                }
            }
            Ok(())
        }
        Err(err) => {
            // The run dies here, but the work need not: the streamed reply
            // is already on stdout and the tool traffic on stderr, and the
            // partial session — everything the turn built up to the
            // failure — lands in a checkpoint the next `--resume` picks
            // up. A provider death mid-delegation is recoverable, not a
            // black hole.
            let path = save
                .clone()
                .unwrap_or_else(|| failed_checkpoint_path(&workspace));
            if let Some(dir) = path.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            match crate::sessions::write_session(&path, agent.session()) {
                Ok(()) => println!(
                    "· checkpoint: the partial session is at {} (resume with --resume {})",
                    path.display(),
                    path.display()
                ),
                Err(write_err) => {
                    eprintln!("warning: the partial session was not checkpointed: {write_err}")
                }
            }
            Err(err.into())
        }
    };
    // Stop-on-exit for the one-shot: a headless run is a session like
    // the TUI's, and the last holder out — here, always this process —
    // stops a derived instance. A setup failure before the turn (an
    // unreadable `--resume`, a backend that will not build) leaves the
    // instance for the next assembly to find: the safe direction is
    // up, never killed under a peer.
    if let Some(mut assembled) = sandboxed {
        assembled.shutdown();
    }
    served
}

/// Where a failed headless turn leaves its partial session when no
/// `--save` was named: under the workspace's `.rhema` — the directory
/// rhema already owns — stamped with the unix time so one failure never
/// overwrites the record of a previous one.
pub(crate) fn failed_checkpoint_path(workspace: &std::path::Path) -> std::path::PathBuf {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_secs())
        .unwrap_or_default();
    workspace
        .join(".rhema")
        .join("runs")
        .join(format!("failed-{secs}.json"))
}
