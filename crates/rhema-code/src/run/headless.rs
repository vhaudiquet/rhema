//! The one-shot runner: `rhema run` turns one prompt with no
//! terminal, the sink that maps model events to stdout/stderr, the
//! provider default models, and the checkpoint a failed turn leaves
//! for `--resume`.

use super::*;

/// The model used headless when neither `--model` nor the stored config
/// names one.
/// The headless event sink: reply text streams raw to stdout (so pipes
/// capture exactly the reply), tool traffic goes to stderr, and the
/// program-status record follows the turn.
pub(crate) struct HeadlessSink {
    /// The intent the status shows: the prompt's first line until a
    /// tool call names a more specific one.
    msg: Option<String>,
}

impl HeadlessSink {
    /// Re-announces the working state from the current tool.
    fn report(&self) {
        status::emit(&Status::Working {
            msg: self.msg.clone(),
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
            ModelEvent::StepStart { .. } => self.report(),
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
/// The program-status record covers the whole run, credential
/// resolution included, and is what survives it: `done` or `error`,
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
    let (mut config, config_loaded) = match Config::load() {
        Ok(config) => (config, true),
        Err(err) => {
            eprintln!("warning: ignoring unreadable rhema config: {err}");
            (Config::default(), false)
        }
    };
    let home = rhema_config::Home::discover().ok();
    let secrets = home.as_ref().map(|home| home.secret_store());
    migrate_stored_keys(&mut config, secrets.as_deref());
    // The same one-way keyring import the TUI runs, for a headless
    // start on a machine upgrading from the old backend.
    if let (Some(home), Some(secrets)) = (&home, &secrets) {
        let imported = config.import_legacy_secrets(
            &rhema_config::secrets::KeyringSecretReader::for_home(home),
            secrets.as_ref(),
        );
        if imported > 0 {
            eprintln!("· copied {imported} key(s) from the OS keyring into the encrypted store");
        }
    }
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
        .unwrap_or_else(|| startup_profile(&config, &workspace, secrets.as_deref()));
    let effective = config.effective(Some(&profile));
    // RHEMA_PROVIDER (inside provider_name) wins over the stored
    // profile, the same env-wins convention the keys follow. Nothing
    // configured is an error naming the fixes: headless has no login
    // screen to fall through to.
    let provider_choice = ProviderChoice::from_stored(effective.provider_name().as_deref())
        .ok_or_else(|| anyhow::anyhow!(crate::app::NO_PROVIDER_HELP))?;
    // The same resolution the TUI's startup runs; a failure names the
    // fix, because headless has no login screen to fall through to.
    let backend = Backend::from_profile_or_help(&effective, secrets.as_deref())
        .map_err(|message| anyhow::anyhow!("{message}"))?;
    // One provider instance serves the main agent and the subagent
    // wrapper: a token-backed adapter keeps one refreshed credential,
    // not a copy per seam.
    let provider = backend.provider();
    let model = args.model.clone().unwrap_or_else(|| {
        effective.model.clone().unwrap_or_else(|| {
            // The provider registry owns the fallback: one table,
            // every frontend, the id the adapter keeps answering for.
            // openrouter rides its own router id here, no longer the
            // copy-pasted google default.
            rhema_config::default_model(provider_choice.stored_name()).to_owned()
        })
    });

    let thinking = args
        .thinking
        .map(rhema_core::request::ThinkingEffort::from)
        .unwrap_or_else(|| effective.thinking_effort());
    let base_system = resolve_base_system(
        rhema_config::Home::discover()
            .ok()
            .map(|home| home.system_prompt())
            .as_deref(),
    );
    let mut agent_config = rhema_host_agent::HostAgentConfig {
        max_turn_steps: args.max_steps,
        transcript: (!args.no_transcript).then(rhema_transcript::TranscriptSettings::default),
        thinking,
        base_system: base_system.clone(),
        disabled_skills: config.skills_disabled.clone(),
        disabled_subagents: config.disabled_subagents.clone(),
        // Approval is intentionally `AllowAll`: commands run inside the
        // sandbox, and the sandbox is the isolation boundary.
        ..rhema_host_agent::HostAgentConfig::new(
            provider.clone(),
            &model,
            &workspace,
            rhema_host_agent::AllowAll,
        )
    };
    // Where bash runs: the sandbox choice engages only off `local`, and
    // before the subagent wire below, their inner shell inherits the
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
                secrets.as_deref(),
                base_system.as_deref(),
            )
            .await?,
        ),
    };
    if let Some(assembled) = &sandboxed {
        assembled.apply_tools(&mut agent_config);
    }
    // Jobs and vision: the same kit placement the TUI runs, so a
    // one-shot's `job` calls land in the same backend.
    if let Some(problem) = rhema_bash::jobs::backend_problem() {
        eprintln!("warning: background jobs disabled: {problem}");
    }
    // Headless has no `/resume`: `--save` names a file, and its stem is
    // the tag a later run reading that file can list. Without one the
    // tag is random and the jobs are not resumable, which the spec says
    // rather than pretending otherwise.
    let jobs_tag = args
        .save
        .as_deref()
        .and_then(crate::jobs::tag_from_path)
        .unwrap_or_else(crate::jobs::fallback_tag);
    // One kit set, one init file: the same placement the TUI runs,
    // kept for this run's lifetime so the file outlives every command.
    let kits_registry = crate::kits::Registry::new();
    let inherited = std::env::var_os("BASH_ENV");
    let mut kits = Vec::new();
    let key = crate::mcp_wire::runtime_session_key(&profile);
    let shared_kits = sandboxed
        .as_ref()
        .and_then(|sandboxed| sandboxed.shared())
        .map(|shared| crate::shared_kit::wire(shared, &key, &jobs_tag));
    let jobs_ephemeral = sandboxed
        .as_ref()
        .is_some_and(|sandboxed| sandboxed.choice == crate::args::SandboxChoice::Ecrou);
    let jobs_wiring = if sandboxed.is_some() {
        if jobs_ephemeral {
            None
        } else {
            shared_kits.as_ref().and_then(|kits| kits.jobs.clone())
        }
    } else {
        rhema_bash::jobs::wire(None, &jobs_tag).cloned()
    };
    if let Some(wiring) = &jobs_wiring {
        kits.push(crate::app::kit_for(
            rhema_bash::runtime::Feature::Jobs,
            wiring.shell_env.clone(),
            vec![crate::app::builtin_stub(
                rhema_bash::jobs::NAME,
                rhema_bash::jobs::VAR_BIN,
                vec![rhema_bash::jobs::MODE],
            )],
            rhema_bash::runtime::Needs::MACRO,
        ));
    }
    // The subscription is the notification feed: the one turn's driver
    // steers from it, so a job that dies mid-run is announced at the
    // next step boundary instead of waiting for a `job ls` that never
    // comes. The one-shot has no idle path: a job outliving the run is
    // the daemon's, and a later run sees it only through `job ls`.
    // The host poller reads this host's gob; a sandboxed run's jobs
    // live in the sandbox's, so no finish feed is possible here.
    let jobs = (sandboxed.is_none() && jobs_wiring.is_some())
        .then(|| crate::jobs::JobsHandle::spawn(jobs_tag.clone()))
        .flatten();
    let vision_wiring = if sandboxed.is_some() {
        shared_kits.as_ref().and_then(|kits| kits.vision.clone())
    } else {
        rhema_bash::vision::wire(None).cloned()
    };
    if let Some(wiring) = &vision_wiring {
        kits.push(crate::app::kit_for(
            rhema_bash::runtime::Feature::Vision,
            wiring.shell_env.clone(),
            vec![crate::app::builtin_stub(
                rhema_bash::vision::NAME,
                rhema_bash::vision::VAR_BIN,
                vec![rhema_bash::vision::MODE],
            )],
            rhema_bash::runtime::Needs::MACRO,
        ));
    }
    let search_wiring = crate::search_wire::wire_for_session(
        &profile,
        &effective,
        secrets.as_deref(),
        sandboxed.as_ref(),
        None,
    );
    if let Some(wiring) = &search_wiring {
        kits.push(crate::app::kit_for(
            rhema_bash::runtime::Feature::Search,
            wiring.shell_env.clone(),
            vec![crate::app::builtin_stub(
                rhema_bash::websearch::NAME,
                rhema_bash::websearch::VAR_BIN,
                vec![rhema_bash::websearch::MODE],
            )],
            rhema_bash::runtime::Needs::HOST_CALL,
        ));
    }
    let decision_wiring = crate::decision_wire::wire_for_session(
        &profile,
        &effective,
        secrets.as_deref(),
        sandboxed.as_ref(),
        None,
    );
    if let Some(wiring) = &decision_wiring {
        kits.push(crate::app::kit_for(
            rhema_bash::runtime::Feature::Decision,
            wiring.shell_env.clone(),
            vec![crate::app::builtin_stub(
                rhema_bash::decision::NAME,
                rhema_bash::decision::VAR_BIN,
                vec![rhema_bash::decision::MODE],
            )],
            rhema_bash::runtime::Needs::HOST_CALL,
        ));
    }
    // The registry outlives the turn: the session dir behind the env
    // must live as long as the commands it provisioned.
    let mcp = crate::mcp_wire::Registry::new();
    let mcp_entry = mcp.wire_for_session(
        &profile,
        &effective,
        &config.disabled_mcp_servers,
        sandboxed.as_ref(),
        None,
    );
    if let Some(entry) = &mcp_entry {
        let mut stubs = vec![crate::app::builtin_stub(
            rhema_mcp::provision::RESERVED,
            rhema_mcp::provision::VAR_BIN,
            vec![rhema_mcp::provision::RESERVED],
        )];
        for line in entry.lines() {
            stubs.push(
                rhema_bash::runtime::Stub::new(
                    line.name.clone(),
                    rhema_mcp::provision::VAR_BIN,
                    vec!["__server".into(), line.server.clone()],
                )
                .expect("the MCP command name passed the provisioner's own check"),
            );
        }
        kits.push(crate::app::kit_for(
            rhema_bash::runtime::Feature::Mcp,
            entry.shell_env().clone(),
            stubs,
            rhema_bash::runtime::Needs::MCP,
        ));
    }
    let placed_env = if kits.is_empty() {
        None
    } else if let Some(shared) = sandboxed.as_ref().and_then(|s| s.shared()) {
        match rhema_sandbox::kit::place_shared(shared, &key, &kits) {
            Ok(placed) => {
                if let Some(sandboxed) = sandboxed.as_ref() {
                    sandboxed.keep_session(placed.session);
                }
                Some(placed.env)
            }
            Err(err) => {
                eprintln!("warning: the sandbox kit placement failed: {err}");
                None
            }
        }
    } else {
        kits_registry
            .place(&kits, inherited.as_deref())
            .map(|placed| placed.env.clone())
    };
    if let Some(env) = placed_env {
        agent_config.shell_env = env;
        if let Some(wiring) = &jobs_wiring {
            agent_config.jobs = Some(wiring.section.to_owned());
        }
        if let Some(wiring) = &vision_wiring {
            agent_config.vision = Some(wiring.section.to_owned());
        }
        if let Some(wiring) = &search_wiring {
            agent_config.websearch = Some(wiring.section.to_owned());
        }
        if let Some(wiring) = &decision_wiring {
            agent_config.decision = Some(wiring.section.to_owned());
        }
        if let Some(entry) = &mcp_entry {
            agent_config.mcp_servers = entry.lines().to_vec();
        }
        if let Some(vision) = &vision_wiring {
            if let Some(tools) = agent_config.tools.as_deref() {
                tools.set_vision_inbox(
                    vision
                        .shell_env
                        .get(rhema_bash::vision::VAR_STAGING)
                        .map(std::path::PathBuf::from),
                );
            }
        }
    }
    agent_config.disabled_mcp_servers = config.disabled_mcp_servers.clone();
    // Subagents ride in as one tool: the built-in general-purpose
    // definition plus whatever the workspace defines.
    // The handle rides to the UI in the wire-up piece; the registry
    // is live either way. The provider factory is the same per-kind
    // answer the TUI carries: one backend per logged-in kind, instance
    // first, so a cross-provider `model` argument reaches the stored
    // proxy or local endpoint headless too.
    agent_config.subagent_providers = Some({
        let profile = effective.clone();
        let secrets = secrets.clone();
        Arc::new(move |kind: &str| {
            Backend::from_kind(kind, &profile, secrets.as_deref()).map(|backend| backend.provider())
        }) as rhema_host_agent::SubagentProviders
    });
    // The prompt's optional sections, the TUI's gate read headless:
    // machine facts for a local session, the sandbox's notice for a
    // mounted one. Before the subagent wire, so the subagents stamp
    // the same sections into their own prompts. The absence list is
    // the user's stderr note, the headless runner's whole user
    // surface, naming the tools this host misses and the ask that
    // installs them, conversationally, through the approval gate.
    let (environment, user, missing) = crate::run::prompt_sections(
        &workspace,
        &sandboxed
            .as_ref()
            .map(|assembled| assembled.workspace.clone())
            .unwrap_or_else(|| workspace.display().to_string()),
        sandboxed
            .as_ref()
            .map(|assembled| assembled.choice.stored_name()),
        &model,
        HARNESS,
    );
    agent_config.environment = environment;
    agent_config.user_preferences = user;
    if !missing.is_empty() {
        eprintln!(
            "note: tools not found: {} (ask the agent to install them)",
            missing.join(", ")
        );
    }
    let (mut agent_config, _subagents) = rhema_subagents::wire(agent_config);
    // The steering slot takes both feeds: subagent finishes first,
    // then background jobs', one chained source.
    if let Some(jobs) = &jobs {
        let mut sources: Vec<Arc<dyn rhema_host_agent::Steering>> =
            agent_config.steering.take().into_iter().collect();
        sources.push(jobs.steering());
        agent_config.steering = Some(Arc::new(rhema_host_agent::SteeringChain::new(sources)));
    }
    if let Some(assembled) = sandboxed.as_ref() {
        assembled.apply_workspace(&mut agent_config);
    }
    // The workspace remembers the profile that served it, the same
    // memory the next launch here reads back. Best effort: a failed
    // write warns without failing the turn. A config that never
    // loaded is never saved: this run's document is defaults, and
    // the write would replace the unreadable file wholesale.
    if config.last_profile_in(&workspace).as_deref() != Some(profile.as_str()) {
        if config_loaded {
            config.note_profile_in(&workspace, &profile);
            if let Err(err) = config.save() {
                eprintln!("warning: the profile choice was not remembered: {err}");
            }
        } else {
            eprintln!(
                "warning: the profile choice was not remembered: {}",
                crate::store::UNLOADED_SAVE_REFUSAL
            );
        }
    }
    let mut session = match &args.resume {
        Some(path) => HostAgent::load_session_from(path)
            .with_context(|| format!("cannot resume session: {}", path.display()))?,
        None => Session::default(),
    };
    // A session written before paste spans existed carries the embedded
    // form in its prompts; the provider must never see a mark (see
    // [`crate::paste`]). The load is the one entry, so one rewrite
    // covers the whole headless run.
    crate::paste::migrate(&mut session.history);
    let mut agent = HostAgent::from_session(agent_config, session)?;

    // The checkpoint, opt-in headless: when `--save` names a file, the
    // engine hands the session over at every stream boundary, before
    // each request, after the assistant message lands and before the
    // first tool runs, after each step's results, so a crash or a kill
    // mid-round, mid-stream included, leaves a resumable file there,
    // which the next `--resume`'s revive closes, instead of no record at
    // all.
    let save = args.save.clone();
    if let Some(path) = &save {
        // `PathStore` creates the parent `0700` on the first commit, so
        // `--save` into a fresh `.rhema/runs/` needs no setup step.
        let store = PathStore::new(path);
        agent.set_commit_hook(Some(Box::new(move |session: &Session| {
            store.save(session).map_err(Into::into)
        })));
    }

    let mut sink = HeadlessSink {
        msg: Some(first_line(&prompt)),
    };
    let outcome: Result<_, anyhow::Error> = match agent.turn(prompt, &mut sink).await {
        Ok(output) if output.stop_reason == rhema_core::StopReason::ToolUse => {
            Err(anyhow::anyhow!(
                "the turn hit its step budget of {} without a final answer; \
             the streamed reply is on stdout",
                args.max_steps
            ))
        }
        Ok(output) => Ok(output),
        Err(err) => Err(err.into()),
    };
    let served: anyhow::Result<()> = match outcome {
        Ok(output) => {
            // Checkpoint after the completed turn: the file's freshest
            // complete state, the stream commits already brought it
            // here, this re-writes it with the final usage attached.
            if let Some(path) = &save
                && let Err(err) = PathStore::new(path).save(agent.session())
            {
                eprintln!("warning: the session was not checkpointed: {err}");
            }
            eprintln!(
                "· {} · ctx {}",
                model,
                crate::screens::format_tokens(Some(crate::app::context_tokens(&output.usage)))
            );
            // The workspace remembers the sandbox kind that just served it,
            // the twin of the profile memory above. Best effort, the same
            // style: a failed write warns without failing the run. A
            // config that never loaded is not saved here either, the
            // profile guard above again.
            if config.last_sandbox_in(&workspace).as_deref() != Some(choice.stored_name()) {
                if config_loaded {
                    config.note_sandbox_in(&workspace, choice.stored_name());
                    if let Err(err) = config.save() {
                        eprintln!("warning: the sandbox choice was not remembered: {err}");
                    }
                } else {
                    eprintln!(
                        "warning: the sandbox choice was not remembered: {}",
                        crate::store::UNLOADED_SAVE_REFUSAL
                    );
                }
            }
            Ok(())
        }
        Err(err) => {
            // The run dies here, but the work need not: the streamed reply
            // is already on stdout and the tool traffic on stderr, and the
            // partial session, everything the turn built up to the
            // failure, lands in a checkpoint the next `--resume` picks
            // up. A provider death mid-delegation is recoverable, not a
            // black hole.
            let path = save
                .clone()
                .unwrap_or_else(|| failed_checkpoint_path(&workspace));
            match PathStore::new(&path).save(agent.session()) {
                Ok(()) => println!(
                    "· checkpoint: the partial session is at {} (resume with --resume {})",
                    path.display(),
                    path.display()
                ),
                Err(write_err) => {
                    eprintln!("warning: the partial session was not checkpointed: {write_err}")
                }
            }
            Err(err)
        }
    };
    // Stop-on-exit for the one-shot: a headless run is a session like
    // the TUI's, and the last holder out, here, always this process,
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
/// `--save` was named: under the workspace's `.rhema`, the directory
/// rhema already owns, stamped with the unix time so one failure never
/// overwrites the record of a previous one.
pub(crate) fn failed_checkpoint_path(workspace: &std::path::Path) -> std::path::PathBuf {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_secs())
        .unwrap_or_default();
    rhema_core::workspace::runs_dir(workspace).join(format!("failed-{secs}.json"))
}
