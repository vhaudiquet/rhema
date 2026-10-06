//! The runner: terminal setup and teardown, the event loop, and the
//! dispatch of the app's effects onto the runtime.
//!
//! TERMINAL OWNERSHIP: the UI thread is the only user of the terminal. The
//! turn and fetch tasks report through the event channel; they never
//! write. Raw mode + alternate screen are entered once here and restored
//! on EVERY exit path — after the event loop returns, and again in the
//! panic hook (restore is idempotent enough: failures are ignored), so a
//! panic never leaves the user inside the alternate screen.

use std::io::stdout;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use crossterm::event::{DisableMouseCapture, EnableMouseCapture};
use crossterm::event::{Event, KeyEventKind};
use crossterm::terminal::{EnterAlternateScreen, LeaveAlternateScreen, enable_raw_mode};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use rhema_coding_agent::{AllowAll, Approval, CodingAgent};
use rhema_config::Config;
use rhema_core::Session;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

use crate::app::Backend;
use crate::app::{App, AppSetup, Effect, RunOptions};
use crate::approval::TuiApproval;
use crate::args::{Args, ProviderChoice};
use crate::store::{ConfigStore, DiskStore};
use crate::turn::{spawn_model_fetch, spawn_turn};
use crate::{events::UiEvent, ui};

/// How long the loop waits for a crossterm event before it re-drains the
/// channel and redraws.
const POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Runs the TUI: resolves credentials, sets up the terminal, and runs the
/// event loop until the app quits. The terminal is restored on every exit
/// path, including panics.
pub async fn run(args: Args) -> anyhow::Result<()> {
    // Credential and session resolution happens BEFORE the terminal setup,
    // so early failures print as plain errors on a normal terminal.
    let store: Arc<dyn ConfigStore> = Arc::new(DiskStore);
    let config = store.load().unwrap_or_else(|err| {
        eprintln!("warning: ignoring unreadable rhema config: {err}");
        Config::default()
    });
    // The stored provider choice decides which backend the TUI runs on;
    // a missing Google key leaves the backend unresolved so the login
    // screen runs.
    let provider_choice = args
        .provider
        .unwrap_or_else(|| ProviderChoice::from_stored(config.provider.as_deref()));
    let backend = match provider_choice {
        ProviderChoice::Google => config
            .google_api_key()
            .map(|api_key| Backend::Google { api_key }),
        ProviderChoice::LlamaCpp => Some(Backend::LlamaCpp {
            url: args
                .llama_url
                .clone()
                .unwrap_or_else(|| config.llama_cpp_url()),
            api_key: config.llama_cpp_api_key(),
        }),
    };
    let session = match &args.resume {
        Some(path) => CodingAgent::load_session_from(path)
            .with_context(|| format!("cannot resume session: {}", path.display()))?,
        None => Session::default(),
    };
    let options = RunOptions {
        theme: args.theme,
        workspace: args.workspace,
        max_steps: args.max_steps,
        no_confirm: args.no_confirm,
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
    crossterm::execute!(stdout(), EnterAlternateScreen, EnableMouseCapture)
        .context("cannot enter the alternate screen")?;
    install_panic_hook();
    let mut terminal = Terminal::new(CrosstermBackend::new(stdout()))
        .context("cannot initialize the terminal backend")?;

    let (tx, mut rx) = unbounded_channel::<UiEvent>();
    let outcome = event_loop(&mut terminal, &mut app, &tx, &mut rx, &mut effects).await;

    restore_terminal();
    outcome
}

/// Restores the terminal: raw mode off, alternate screen left. Errors are
/// ignored so a restore attempt never panics (it runs in the panic hook).
fn restore_terminal() {
    let _ = crossterm::terminal::disable_raw_mode();
    let _ = crossterm::execute!(stdout(), DisableMouseCapture, LeaveAlternateScreen);
    use std::io::Write;
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
            let (approval, answers) = if app.options.no_confirm {
                (Arc::new(AllowAll) as Arc<dyn Approval>, None)
            } else {
                let (hook, answerer) = TuiApproval::pair(tx.clone());
                (Arc::new(hook) as Arc<dyn Approval>, Some(answerer))
            };
            app.begin_turn(answers);
            spawn_turn(agent, input, approval, app.options.save.clone(), tx.clone());
        }
    }
}
