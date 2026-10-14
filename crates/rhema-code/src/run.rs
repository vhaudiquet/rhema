//! The runner: terminal setup and teardown, the event loop, and the
//! dispatch of the app's effects onto the runtime.
//!
//! TERMINAL OWNERSHIP: the UI thread is the only user of the terminal. The
//! turn and fetch tasks report through the event channel; they never
//! write. Raw mode + alternate screen are entered once here and restored
//! on EVERY exit path — after the event loop returns, and again in the
//! panic hook (restore is idempotent enough: failures are ignored), so a
//! panic never leaves the user inside the alternate screen.

use std::io::{IsTerminal, stdout};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture, Event,
    KeyEventKind, KeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::terminal::{EnterAlternateScreen, LeaveAlternateScreen, enable_raw_mode};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use rhema_acp::AcpOptions;
use rhema_coding_agent::{AllowAll, Approval, CodingAgent};
use rhema_config::Config;
use rhema_config::secrets::KeyringSecretStore;
use rhema_core::{Event as ModelEvent, EventSink, Session};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

use crate::app::Backend;
use crate::app::{App, AppSetup, Effect, RunOptions, startup_profile};
use crate::args::{Args, ProviderChoice, RunArgs, SystemCommand};
use crate::sessions;
use crate::status::{self, Status, Tracker, first_line};
use crate::store::{ConfigStore, DiskStore};
use crate::turn::{spawn_model_fetch, spawn_turn, spawn_usage_fetch};
use crate::{events::UiEvent, ui};

/// How long the loop waits for a crossterm event before it re-drains the
/// channel and redraws.
const POLL_INTERVAL: Duration = Duration::from_millis(50);

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
struct HeadlessSink {
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
fn read_prompt_from_stdin() -> anyhow::Result<String> {
    use std::io::Read;
    let mut text = String::new();
    std::io::stdin().read_to_string(&mut text)?;
    Ok(text)
}

/// The user's SYSTEM.md as the base prompt when the file exists;
/// `None` falls back to the built-in default. Nothing is ever written:
/// the file is an opt-in override, not a created one.
fn resolve_base_system(path: Option<&std::path::Path>) -> Option<String> {
    let path = path?;
    std::fs::read_to_string(path).ok()
}

/// Moves the config's stored literal keys into the system keychain,
/// best effort, rewriting the file so the move is recorded. Machines
/// without a keychain keep their literals — the pre-keychain behavior,
/// retried here on every start.
fn migrate_stored_keys(config: &mut Config) {
    let moved = config.migrate_secrets(&KeyringSecretStore::new());
    if moved == 0 {
        return;
    }
    eprintln!("· moved {moved} provider key(s) into the system keychain");
    if let Err(err) = config.save() {
        eprintln!("warning: the config file still names the moved keys: {err}");
    }
}

/// Runs the frontend: the ACP server, the `run` one-shot, or the
/// full-screen TUI — the subcommand decides, and bare `rhema` is the TUI
/// only when a terminal is attached.
pub async fn run(args: Args) -> anyhow::Result<()> {
    // The ACP branch first: stdin/stdout are the protocol wire, so no
    // terminal detection may run — a served agent never has a tty.
    match args.command {
        Some(crate::args::Command::Acp(acp)) => return run_acp(acp),
        Some(crate::args::Command::Run(run)) => return run_headless(run).await,
        Some(crate::args::Command::System { action }) => {
            return system_command(action, &args.workspace);
        }
        None => {}
    }
    // Headless is explicit now: a piped stdout alone is not an
    // instruction to run a prompt, it is usually a mistake worth
    // catching early.
    if !stdout().is_terminal() {
        anyhow::bail!(
            "stdout is not a terminal: nothing to start interactively — \
             use `rhema run` for the one-shot runner"
        );
    }
    run_tui(args).await
}

/// Runs the ACP server: stdin/stdout become the protocol wire and the
/// call blocks until the client hangs up. The backend the sessions run
/// on resolves once, from the flags and the stored config.
fn run_acp(args: crate::args::AcpArgs) -> anyhow::Result<()> {
    rhema_acp::serve(rhema_acp::AcpOptions {
        profile: args.profile,
        model: args.model,
        max_steps: Some(args.max_steps),
        thinking: args.thinking.map(From::from),
        ..AcpOptions::default()
    })
}

/// The `rhema system` utilities: they shape what turns would send and
/// run no model — no provider, profile, or credential is touched.
fn system_command(action: SystemCommand, workspace: &std::path::Path) -> anyhow::Result<()> {
    match action {
        SystemCommand::Edit => system_edit_at(&Config::system_prompt_path()?, None),
        SystemCommand::Restore => system_restore_at(&Config::system_prompt_path()?),
        SystemCommand::Show => {
            let prompt = system_show_at(Config::system_prompt_path().ok().as_deref(), workspace)?;
            print!("{prompt}");
            Ok(())
        }
    }
}

/// `rhema system edit`: seeds the config's SYSTEM.md with the built-in
/// default when missing, then opens the editor on it. `editor` — the
/// tests' seam — overrides `$EDITOR`/`$VISUAL`.
fn system_edit_at(path: &std::path::Path, editor: Option<&str>) -> anyhow::Result<()> {
    if !path.exists() {
        seed_system_file(path)?;
        eprintln!("· seeded {} with the built-in default", path.display());
    }
    run_editor(path, editor)
}

/// `rhema system restore`: the config's SYSTEM.md becomes the built-in
/// default again.
fn system_restore_at(path: &std::path::Path) -> anyhow::Result<()> {
    seed_system_file(path)?;
    eprintln!("· restored {} to the built-in default", path.display());
    Ok(())
}

/// The prompt the next turn in `workspace` would send: the config's
/// SYSTEM.md when present, the built-in default otherwise, through the
/// same assembly the agent stamps — rendered over the workspace, its
/// skills, and the tools.
fn system_show_at(
    path: Option<&std::path::Path>,
    workspace: &std::path::Path,
) -> anyhow::Result<String> {
    let workspace = std::fs::canonicalize(workspace).unwrap_or_else(|_| workspace.to_path_buf());
    Ok(rhema_coding_agent::stamped_system(
        resolve_base_system(path).as_deref(),
        &workspace,
    )?)
}

/// Writes the built-in default to `path`, creating the directory. The
/// default goes out verbatim: the file is a template, injections
/// render per turn, never here.
fn seed_system_file(path: &std::path::Path) -> anyhow::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, rhema_coding_agent::DEFAULT_SYSTEM_MD)?;
    Ok(())
}

/// Runs the editor on `path`, blocking until it closes. The editor
/// value may carry flags (`code -w`), so it goes through `sh -c` with
/// the file as the only positional argument; a non-zero exit fails.
fn run_editor(path: &std::path::Path, editor: Option<&str>) -> anyhow::Result<()> {
    let editor = match editor {
        Some(editor) => editor.to_owned(),
        None => match (std::env::var("EDITOR"), std::env::var("VISUAL")) {
            (Ok(value), _) if !value.trim().is_empty() => value,
            (_, Ok(value)) if !value.trim().is_empty() => value,
            _ => anyhow::bail!("no editor: set $EDITOR"),
        },
    };
    let status = std::process::Command::new("sh")
        .arg("-c")
        .arg(format!("exec {editor} \"$@\""))
        .arg(&editor)
        .arg(path)
        .status()
        .map_err(|err| anyhow::anyhow!("failed to run editor {editor:?}: {err}"))?;
    if !status.success() {
        anyhow::bail!("editor {editor:?} exited with {status}");
    }
    Ok(())
}

/// The headless one-shot: resolve the backend, run one turn, print the
/// reply to stdout and everything else to stderr. Built for pipes.
///
/// The program-status record covers the whole run — credential
/// resolution included — and is what survives it: `done` or `error`,
/// never cleared (see `crate::status`). A usage failure (no prompt)
/// exits before any record exists.
async fn run_headless(args: RunArgs) -> anyhow::Result<()> {
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
async fn run_prompt(args: RunArgs, prompt: String) -> anyhow::Result<()> {
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
        .or_else(|| effective.thinking_effort())
        .unwrap_or(rhema_core::request::ThinkingEffort::Default);
    let agent_config = rhema_coding_agent::CodingAgentConfig {
        max_turn_steps: args.max_steps,
        thinking,
        base_system: resolve_base_system(Config::system_prompt_path().ok().as_deref()),
        ..rhema_coding_agent::CodingAgentConfig::new(&model, &workspace)
    };
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

    let mut sink = HeadlessSink {
        msg: Some(first_line(&prompt)),
        step: None,
        max_steps: args.max_steps,
    };
    let output = agent.turn(prompt, &mut sink, approval).await?;
    eprintln!(
        "· {} · ctx {}",
        model,
        crate::screens::format_tokens(Some(crate::app::context_tokens(&output.usage)))
    );
    Ok(())
}

/// Runs the TUI: resolves credentials, sets up the terminal, and runs the
/// event loop until the app quits. The terminal is restored on every exit
/// path, including panics.
async fn run_tui(args: Args) -> anyhow::Result<()> {
    // Credential and session resolution happens BEFORE the terminal setup,
    // so early failures print as plain errors on a normal terminal.
    let store: Arc<dyn ConfigStore> = Arc::new(DiskStore);
    let mut config = store.load().unwrap_or_else(|err| {
        eprintln!("warning: ignoring unreadable rhema config: {err}");
        Config::default()
    });
    migrate_stored_keys(&mut config);
    // The session's keychain: startup resolution reads it, and the app
    // places fresh logins in it and cleans up after removals.
    let secrets: Arc<dyn rhema_config::secrets::SecretStore> = Arc::new(KeyringSecretStore::new());
    // Without an explicit --profile the session starts on the profile
    // this workspace last used, or on the default/first one that can
    // serve turns — a logged-in profile never opens the login screen.
    // The stored provider choice then decides which backend runs; a
    // missing key still leaves the backend unresolved so the login
    // screen runs.
    // The real path, before anything moves into the app: the profile
    // memory and the saved sessions group by it, so a relative "." must
    // not become a "." directory.
    let workspace =
        std::fs::canonicalize(&args.workspace).unwrap_or_else(|_| args.workspace.clone());
    let profile = args
        .profile
        .clone()
        .unwrap_or_else(|| startup_profile(&config, &workspace, Some(&*secrets)));
    let effective = config.effective(Some(&profile));
    // RHEMA_PROVIDER (inside provider_name) wins over the stored
    // profile, the same env-wins convention the keys follow. Nothing
    // configured (or a spelling no provider answers to) leaves the
    // backend unresolved: the login screen opens, nothing defaults.
    let backend = ProviderChoice::from_stored(effective.provider_name().as_deref()).and_then(
        |provider_choice| match provider_choice {
            ProviderChoice::Google => effective
                .api_key("google", Some(&*secrets))
                .map(|api_key| Backend::Google { api_key }),
            ProviderChoice::LlamaCpp => Some(Backend::LlamaCpp {
                url: effective.llama_cpp_url(),
                api_key: effective.api_key("llama-cpp", Some(&*secrets)),
            }),
            ProviderChoice::OpenRouter => effective
                .api_key("openrouter", Some(&*secrets))
                .map(|api_key| Backend::OpenRouter { api_key }),
            ProviderChoice::Anthropic => effective
                .api_key("anthropic", Some(&*secrets))
                .map(|api_key| Backend::Anthropic { api_key }),
            ProviderChoice::OpenAi => {
                effective
                    .api_key("openai", Some(&*secrets))
                    .map(|api_key| Backend::OpenAi {
                        api_key,
                        base_url: effective.openai_base_url(),
                    })
            }
            ProviderChoice::DeepSeek => effective
                .api_key("deepseek", Some(&*secrets))
                .map(|api_key| Backend::DeepSeek { api_key }),
            ProviderChoice::Zai => {
                effective
                    .api_key("zai", Some(&*secrets))
                    .map(|api_key| Backend::Zai {
                        api_key,
                        // A stored instance may override the route (a proxy);
                        // none means the Coding Plan endpoint itself.
                        base_url: effective
                            .instance_for_provider("zai")
                            .and_then(|(_, instance)| effective.instance_url(instance)),
                    })
            }
            ProviderChoice::Copilot => effective
                .api_key("copilot", Some(&*secrets))
                .map(|api_key| Backend::Copilot { api_key }),
        },
    );
    let session = match &args.resume {
        Some(path) => CodingAgent::load_session_from(path)
            .with_context(|| format!("cannot resume session: {}", path.display()))?,
        None => Session::default(),
    };
    let agent_base = resolve_base_system(Config::system_prompt_path().ok().as_deref());
    let thinking = args
        .thinking
        .map(rhema_core::request::ThinkingEffort::from)
        .or_else(|| config.effective(Some(&profile)).thinking_effort())
        .unwrap_or(rhema_core::request::ThinkingEffort::Default);
    // The look and feel is a machine choice, not a per-invocation one:
    // the stored theme and icon flavor decide, unparseable values read
    // as the defaults.
    let theme = match config.theme.as_deref() {
        Some("light") => crate::args::ThemeKind::Light,
        _ => crate::args::ThemeKind::Dark,
    };
    let icons = config
        .icons
        .as_deref()
        .and_then(crate::icons::Flavor::parse)
        .unwrap_or_default();
    let options = RunOptions {
        theme,
        icons,
        profile: Some(profile),
        workspace: workspace.clone(),
        max_steps: args.max_steps,
        thinking,
        base_system: agent_base,
        save: args.save,
    };
    let (mut app, mut effects) = {
        let app = App::new(AppSetup {
            store,
            config,
            backend,
            session,
            options,
            secrets: Some(Arc::clone(&secrets)),
        })?;
        let effects = app.initial_effects();
        (app, effects)
    };

    enable_raw_mode().context("the rhema-tui binary needs a terminal; pipe output through a pty or use the rhema CLI instead")?;
    // Shift-enter is a distinct key only when the terminal encodes
    // every key: under the kitty protocol the disambiguate flag alone
    // leaves plain shift+key in legacy mode, so shift+enter would stay
    // an indistinguishable CR. All-keys plus alternate keys make
    // Ghostty, kitty, and WezTerm report shift-enter as CSI 13;2u and
    // keep shifted letters correct; terminals without the protocol
    // ignore the push, and newlines keep working through alt-enter and
    // ctrl-j.
    // Associated text is not optional here: all-keys mode alone strips
    // text from the stream, and an AltGr key's codepoint is its base
    // key — `@` on AZERTY (AltGr+0) arrives as `à` plus Ctrl+Alt. Only
    // the associated-text field names the composed character, so a
    // text-driven reader without it loses every AltGr character and
    // dead key. The parser decoding it lives in the patched crossterm
    // (see [patch.crates-io] in the workspace Cargo.toml).
    // The flags are pushed after the alternate screen is entered and
    // popped before it is left: the protocol's lifetime is paired with
    // the screen it was pushed on, and a terminal keeping per-screen
    // stacks would never see a pop issued across the swap.
    crossterm::execute!(
        stdout(),
        EnterAlternateScreen,
        PushKeyboardEnhancementFlags(
            KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                | KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS
                | KeyboardEnhancementFlags::REPORT_ALL_KEYS_AS_ESCAPE_CODES
                | KeyboardEnhancementFlags::REPORT_ASSOCIATED_TEXT
        ),
        EnableMouseCapture,
        EnableBracketedPaste
    )
    .context("cannot enter the alternate screen")?;
    install_panic_hook();
    let mut terminal = Terminal::new(CrosstermBackend::new(stdout()))
        .context("cannot initialize the terminal backend")?;

    let (tx, mut rx) = unbounded_channel::<UiEvent>();
    let outcome = event_loop(&mut terminal, &mut app, &tx, &mut rx, &mut effects).await;

    restore_terminal();
    // Best-effort save for /resume, after the terminal is back: an empty
    // session saves nothing, and a failed save warns without failing the
    // exit. A resumed session writes back to its own file, so continuing
    // it never grows a second entry.
    if let Some((session, origin)) = app.export_session() {
        let saved = match origin {
            Some(path) => sessions::write_session(&path, &session),
            None => sessions::base_dir()
                .and_then(|base| sessions::save_session(&base, &workspace, &session))
                .map(|_| ()),
        };
        if let Err(err) = saved {
            eprintln!("warning: the session was not saved for /resume: {err}");
        }
    }
    outcome
}

/// Restores the terminal: raw mode off, alternate screen left. Errors are
/// ignored so a restore attempt never panics (it runs in the panic hook).
fn restore_terminal() {
    let _ = crossterm::terminal::disable_raw_mode();
    // Each step writes on its own so one failure cannot skip the rest.
    // The pops are the canonical bare `CSI < u` (crossterm's command
    // emits the numbered `CSI <1u`, which a terminal may ignore), and
    // the trailing push of zero flags masks any entry the pops could not
    // remove — the effective encoding lands on legacy either way. All
    // of it runs on the alternate screen, where the push happened.
    use std::io::Write;
    let _ = write!(stdout(), "\x1b[<u");
    let _ = stdout().flush();
    let _ = write!(stdout(), "\x1b[<u");
    let _ = stdout().flush();
    let _ = write!(stdout(), "\x1b[>0u");
    let _ = stdout().flush();
    let _ = crossterm::execute!(
        stdout(),
        DisableBracketedPaste,
        DisableMouseCapture,
        LeaveAlternateScreen
    );
    let _ = stdout().flush();
    // The program-status record goes with the session: an exited
    // interactive rhema leaves no badge behind (terminals that already
    // dropped the record on exit ignore this).
    status::clear();
}

/// Installs a panic hook that restores the terminal before the original
/// hook prints the panic message, so the message lands on the normal
/// screen.
fn install_panic_hook() {
    let original = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_terminal();
        original(info);
    }));
}

/// The UI thread's loop: draw, drain the event channel, poll crossterm.
/// Nothing here ever blocks on a task — that is the approval bridge's
/// deadlock-safety invariant (see `crate::approval`).
async fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    app: &mut App,
    tx: &UnboundedSender<UiEvent>,
    rx: &mut UnboundedReceiver<UiEvent>,
    effects: &mut Vec<Effect>,
) -> anyhow::Result<()> {
    let mut status = Tracker::default();
    for effect in std::mem::take(effects) {
        dispatch(app, tx, &mut status, effect);
    }
    loop {
        status.sync(app);
        terminal.draw(|frame| ui::render(app, frame))?;

        while let Ok(event) = rx.try_recv() {
            status.observe(&event);
            for effect in app.on_event(event) {
                dispatch(app, tx, &mut status, effect);
            }
            if app.should_quit {
                return Ok(());
            }
        }
        if app.should_quit {
            return Ok(());
        }

        if crossterm::event::poll(POLL_INTERVAL)? {
            match crossterm::event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    for effect in app.on_key(key) {
                        dispatch(app, tx, &mut status, effect);
                    }
                }
                Event::Mouse(mouse) => match mouse.kind {
                    crossterm::event::MouseEventKind::ScrollUp => app.on_mouse_scroll(true),
                    crossterm::event::MouseEventKind::ScrollDown => app.on_mouse_scroll(false),
                    // A click is the button's release: press-drag-release
                    // lands once, on the row the user let go over.
                    crossterm::event::MouseEventKind::Up(crossterm::event::MouseButton::Left) => {
                        for effect in app.on_click(mouse.column, mouse.row) {
                            dispatch(app, tx, &mut status, effect);
                        }
                    }
                    _ => {}
                },
                Event::Paste(text) => app.on_paste(&text),
                _ => {}
            }
        }
    }
}

/// Performs one effect: spawn a fetch task or a turn task. The turn takes
/// the agent off the UI thread until its `TurnFinished` event.
fn dispatch(app: &mut App, tx: &UnboundedSender<UiEvent>, status: &mut Tracker, effect: Effect) {
    match effect {
        Effect::FetchModels(why) => {
            // A login validates one provider; the catalogs otherwise span
            // every instance the profile holds credentials for.
            let backends = match &why {
                crate::events::FetchWhy::Login { backend, .. } => {
                    vec![crate::app::Candidate {
                        instance: None,
                        backend: backend.clone(),
                    }]
                }
                _ => Backend::candidates_from_profile(&app.effective(), app.secret_store()),
            };
            if backends.is_empty() {
                return;
            }
            spawn_model_fetch(backends, why, tx.clone());
        }
        Effect::FetchUsage => {
            // One allowance per session: the active backend's only, and
            // the fetch failing just keeps the badge off.
            let Some(backend) = app.backend.clone() else {
                return;
            };
            spawn_usage_fetch(backend, tx.clone());
        }
        Effect::StartTurn(input) => {
            let Some(agent) = app.agent.take() else {
                return;
            };
            let (cancel, cancelled) = tokio::sync::watch::channel(false);
            // The status names the prompt until a tool call takes over.
            status.note_prompt(&input);
            // Auto-approved for now: the Approval seam — and the modal
            // wired through it — is where command allowlists will land.
            app.begin_turn(cancel, None);
            spawn_turn(
                agent,
                input,
                Arc::new(AllowAll),
                app.options.save.clone(),
                cancelled,
                tx.clone(),
            );
        }
        Effect::StartLoginFlow => {
            let provider = match &app.screen {
                crate::app::Screen::Settings(settings) => settings
                    .dialog
                    .as_ref()
                    .map(|dialog| dialog.selected_provider()),
                _ => None,
            };
            // The dialog already flipped to the flow stage; the effect
            // only builds the flow. Copilot's device grant is the first
            // user; further flow providers join behind the same arm.
            let flow: Box<dyn rhema_provider::InteractiveLogin> = match provider {
                Some(crate::screens::LoginProvider::Copilot) => {
                    Box::new(rhema_provider_copilot::CopilotLogin::new())
                }
                _ => return,
            };
            let (cancel, cancelled) = tokio::sync::watch::channel(false);
            app.login_cancel = Some(cancel);
            crate::turn::spawn_login_flow(flow, cancelled, tx.clone());
        }
    }
}

#[cfg(test)]
mod system_tests {
    use super::*;

    #[test]
    fn edit_seeds_the_default_when_the_file_is_missing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("SYSTEM.md");

        system_edit_at(&path, Some("true")).unwrap();

        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            rhema_coding_agent::DEFAULT_SYSTEM_MD
        );
    }

    #[test]
    fn edit_keeps_an_existing_file() {
        // Seeding is for the missing file only; an existing SYSTEM.md,
        // even an empty one, is the user's.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("SYSTEM.md");
        std::fs::write(&path, "mine").unwrap();

        system_edit_at(&path, Some("true")).unwrap();

        assert_eq!(std::fs::read_to_string(&path).unwrap(), "mine");
    }

    #[test]
    fn edit_fails_when_the_editor_exits_non_zero() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("SYSTEM.md");
        assert!(system_edit_at(&path, Some("false")).is_err());
    }

    #[test]
    fn restore_overwrites_with_the_default_and_creates_the_directory() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("SYSTEM.md");

        system_restore_at(&path).unwrap();

        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            rhema_coding_agent::DEFAULT_SYSTEM_MD
        );
    }

    #[test]
    fn show_renders_the_base_over_the_workspace_skills_and_tools() {
        let config = tempfile::tempdir().unwrap();
        let base = config.path().join("SYSTEM.md");
        std::fs::write(
            &base,
            "ws {{ workspace }}{% for skill in skills %} skill {{ skill.name }}{% endfor %}{% for tool in tools %} tool {{ tool.name }}{% endfor %}",
        )
        .unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let folder = workspace.path().join(".rhema").join("skills").join("local");
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(
            folder.join("SKILL.md"),
            "---\ndescription: local trigger\n---\nbody",
        )
        .unwrap();

        let prompt = system_show_at(Some(&base), workspace.path()).unwrap();

        let canonical = std::fs::canonicalize(workspace.path()).unwrap();
        assert!(
            prompt.starts_with(&format!("ws {}", canonical.display())),
            "{prompt}"
        );
        assert!(prompt.contains("skill local"), "{prompt}");
        assert!(prompt.contains("tool bash"), "{prompt}");
    }

    #[test]
    fn show_falls_back_to_the_built_in_default_without_a_file() {
        let workspace = tempfile::tempdir().unwrap();

        let prompt = system_show_at(None, workspace.path()).unwrap();

        assert!(prompt.contains("bash"), "{prompt}");
        assert!(!prompt.contains("{{"), "the default is rendered: {prompt}");
    }
}
