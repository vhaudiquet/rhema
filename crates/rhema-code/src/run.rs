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
use rhema_coding_agent::{AllowAll, Approval, CodingAgent};
use rhema_config::Config;
use rhema_core::{Event as ModelEvent, EventSink, Session};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

use crate::app::Backend;
use crate::app::{App, AppSetup, Effect, RunOptions, startup_profile};
use crate::args::{Args, ProviderChoice};
use crate::sessions;
use crate::store::{ConfigStore, DiskStore};
use crate::turn::{spawn_model_fetch, spawn_turn};
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

/// The headless event sink: reply text streams raw to stdout (so pipes
/// capture exactly the reply), tool traffic goes to stderr.
struct HeadlessSink;

impl EventSink for HeadlessSink {
    fn emit(&mut self, event: ModelEvent<'_>) {
        use std::io::Write;
        match event {
            ModelEvent::TextDelta(delta) => {
                print!("{delta}");
                let _ = std::io::stdout().flush();
            }
            ModelEvent::ToolCallStart { name, .. } => {
                eprintln!("⌁ {name}");
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

/// First line of a tool result, cut for the headless stderr stream.
fn first_line(content: &str) -> String {
    let line = content.lines().next().unwrap_or("");
    let mut cut = line.chars().take(72).collect::<String>();
    if line.chars().count() > 72 {
        cut.push('…');
    }
    cut
}

/// Humanizes a token count: 1_048_576 → "1.0M", 131_072 → "131k".
fn format_tokens(limit: u32) -> String {
    let limit = u64::from(limit);
    if limit >= 1_000_000 {
        format!("{:.1}M", limit as f64 / 1_000_000.0)
    } else if limit >= 10_000 {
        format!("{}k", limit / 1_000)
    } else {
        format!("{limit}")
    }
}

/// The user's SYSTEM.md as the base prompt when the file exists;
/// `None` falls back to the built-in default. Nothing is ever written:
/// the file is an opt-in override, not a created one.
fn resolve_base_system(path: Option<&std::path::Path>) -> Option<String> {
    let path = path?;
    std::fs::read_to_string(path).ok()
}

/// Runs the frontend: headless one-shot when a prompt or a pipe asks for
/// it, the full-screen TUI otherwise.
pub async fn run(args: Args) -> anyhow::Result<()> {
    use std::io::IsTerminal;
    let headless = args.print.is_some()
        || args.print == Some("-".to_owned())
        || !std::io::stdout().is_terminal();
    // A piped stdin is a prompt source, not a REPL: without a prompt and
    // without a terminal there is nothing interactive to do.
    if headless {
        return run_headless(args).await;
    }
    run_tui(args).await
}

/// The headless one-shot: resolve the backend, run one turn, print the
/// reply to stdout and everything else to stderr. Built for pipes.
async fn run_headless(args: Args) -> anyhow::Result<()> {
    let prompt = match args.print.as_deref() {
        Some("-") => {
            use std::io::Read;
            let mut text = String::new();
            std::io::stdin().read_to_string(&mut text)?;
            text
        }
        Some(text) => text.to_owned(),
        None if !std::io::stdin().is_terminal() => {
            use std::io::Read;
            let mut text = String::new();
            std::io::stdin().read_to_string(&mut text)?;
            text
        }
        None => anyhow::bail!(
            "no prompt: pass --print \"text\" (or --print - with the prompt on stdin)"
        ),
    };
    if prompt.trim().is_empty() {
        anyhow::bail!("empty prompt");
    }

    let mut config = match Config::load() {
        Ok(config) => config,
        Err(err) => {
            eprintln!("warning: ignoring unreadable rhema config: {err}");
            Config::default()
        }
    };
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
        .unwrap_or_else(|| startup_profile(&config, &workspace));
    let effective = config.effective(Some(&profile));
    let provider_choice = args
        .provider
        .unwrap_or_else(|| ProviderChoice::from_stored(effective.provider.as_deref()));
    // No login screen exists headless: a missing credential is an error
    // naming the fix.
    let backend = match provider_choice {
        ProviderChoice::Google => effective
            .google_api_key()
            .map(|api_key| Backend::Google { api_key })
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "missing API key: set GEMINI_API_KEY (or log in once interactively)"
                )
            })?,
        ProviderChoice::LlamaCpp => Backend::LlamaCpp {
            url: args
                .llama_url
                .clone()
                .unwrap_or_else(|| effective.llama_cpp_url()),
            api_key: effective.llama_cpp_api_key(),
        },
        ProviderChoice::OpenRouter => Backend::OpenRouter {
            api_key: effective
                .openrouter_api_key()
                .ok_or_else(|| anyhow::anyhow!("missing API key: set OPENROUTER_API_KEY"))?,
        },
        ProviderChoice::Anthropic => Backend::Anthropic {
            api_key: effective.anthropic_api_key().ok_or_else(|| {
                anyhow::anyhow!(
                    "missing API key: set ANTHROPIC_API_KEY (or log in once interactively)"
                )
            })?,
        },
        ProviderChoice::OpenAi => Backend::OpenAi {
            api_key: effective.openai_api_key().ok_or_else(|| {
                anyhow::anyhow!(
                    "missing API key: set OPENAI_API_KEY (or log in once interactively)"
                )
            })?,
        },
    };
    let model = args.model.clone().unwrap_or_else(|| {
        effective
            .model
            .clone()
            .unwrap_or_else(|| match provider_choice {
                ProviderChoice::LlamaCpp => DEFAULT_LLAMA_MODEL.to_owned(),
                ProviderChoice::Anthropic => DEFAULT_ANTHROPIC_MODEL.to_owned(),
                ProviderChoice::OpenAi => DEFAULT_OPENAI_MODEL.to_owned(),
                ProviderChoice::Google | ProviderChoice::OpenRouter => DEFAULT_MODEL.to_owned(),
            })
    });

    let workspace =
        std::fs::canonicalize(&args.workspace).unwrap_or_else(|_| args.workspace.clone());
    let agent_config = rhema_coding_agent::CodingAgentConfig {
        max_turn_steps: args.max_steps,
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

    let mut sink = HeadlessSink;
    let output = agent.turn(prompt, &mut sink, approval).await?;
    eprintln!(
        "· {} · ctx {}",
        model,
        format_tokens(output.usage.input_tokens)
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
    let config = store.load().unwrap_or_else(|err| {
        eprintln!("warning: ignoring unreadable rhema config: {err}");
        Config::default()
    });
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
        .unwrap_or_else(|| startup_profile(&config, &workspace));
    let effective = config.effective(Some(&profile));
    let provider_choice = args
        .provider
        .unwrap_or_else(|| ProviderChoice::from_stored(effective.provider.as_deref()));
    let backend = match provider_choice {
        ProviderChoice::Google => effective
            .google_api_key()
            .map(|api_key| Backend::Google { api_key }),
        ProviderChoice::LlamaCpp => Some(Backend::LlamaCpp {
            url: args
                .llama_url
                .clone()
                .unwrap_or_else(|| effective.llama_cpp_url()),
            api_key: effective.llama_cpp_api_key(),
        }),
        ProviderChoice::OpenRouter => effective
            .openrouter_api_key()
            .map(|api_key| Backend::OpenRouter { api_key }),
        ProviderChoice::Anthropic => effective
            .anthropic_api_key()
            .map(|api_key| Backend::Anthropic { api_key }),
        ProviderChoice::OpenAi => effective
            .openai_api_key()
            .map(|api_key| Backend::OpenAi { api_key }),
    };
    let session = match &args.resume {
        Some(path) => CodingAgent::load_session_from(path)
            .with_context(|| format!("cannot resume session: {}", path.display()))?,
        None => Session::default(),
    };
    let agent_base = resolve_base_system(Config::system_prompt_path().ok().as_deref());
    let options = RunOptions {
        theme: args.theme,
        profile: Some(profile),
        workspace: workspace.clone(),
        max_steps: args.max_steps,
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
    for effect in std::mem::take(effects) {
        dispatch(app, tx, effect);
    }
    loop {
        terminal.draw(|frame| ui::render(app, frame))?;

        while let Ok(event) = rx.try_recv() {
            for effect in app.on_event(event) {
                dispatch(app, tx, effect);
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
                        dispatch(app, tx, effect);
                    }
                }
                Event::Mouse(mouse) => match mouse.kind {
                    crossterm::event::MouseEventKind::ScrollUp => app.on_mouse_scroll(true),
                    crossterm::event::MouseEventKind::ScrollDown => app.on_mouse_scroll(false),
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
fn dispatch(app: &mut App, tx: &UnboundedSender<UiEvent>, effect: Effect) {
    match effect {
        Effect::FetchModels(why) => {
            let backend = match &why {
                crate::events::FetchWhy::Login { backend } => Some(backend.clone()),
                _ => app.backend.clone(),
            };
            let Some(backend) = backend else {
                return;
            };
            spawn_model_fetch(backend, why, tx.clone());
        }
        Effect::StartTurn(input) => {
            let Some(agent) = app.agent.take() else {
                return;
            };
            let (cancel, cancelled) = tokio::sync::watch::channel(false);
            app.turn_cancel = Some(cancel);
            // Auto-approved for now: the Approval seam — and the modal
            // wired through it — is where command allowlists will land.
            app.begin_turn(None);
            spawn_turn(
                agent,
                input,
                Arc::new(AllowAll),
                app.options.save.clone(),
                cancelled,
                tx.clone(),
            );
        }
    }
}
