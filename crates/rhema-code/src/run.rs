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
    KeyEventKind, KeyboardEnhancementFlags, PopKeyboardEnhancementFlags,
    PushKeyboardEnhancementFlags,
};
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use rhema_acp::AcpOptions;
use rhema_coding_agent::{AllowAll, Approval, CodingAgent};
use rhema_config::Config;
use rhema_config::secrets::KeyringSecretStore;
use rhema_core::{Event as ModelEvent, EventSink, Session};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

use crate::app::Backend;
use crate::app::{App, AppSetup, Effect, RunOptions, startup_profile, startup_sandbox};
use crate::args::{Args, ProviderChoice, RunArgs, SystemCommand};
use crate::sessions;
use crate::status::{self, Status, Tracker, first_line};
use crate::store::{ConfigStore, DiskStore};
use crate::turn::{spawn_model_fetch, spawn_session_resume, spawn_turn, spawn_usage_fetch};
use crate::{events::UiEvent, ui};

mod headless;
mod system;
#[cfg(test)]
mod tests;

use headless::run_headless;
pub(crate) use system::HARNESS;
use system::{run_editor, system_command};

/// How long the loop waits for a crossterm event before it re-drains the
/// channel and redraws.
const POLL_INTERVAL: Duration = Duration::from_millis(50);

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
        // The MCP CLI owns the process: its exit code IS ours, and
        // its stdout/stderr contract is the model's output contract —
        // nothing rhema adds to either. The argv rides with the mode
        // as element zero, the dispatch's convention.
        Some(crate::args::Command::Mcp { args: rest }) => {
            let mut argv = Vec::with_capacity(rest.len() + 1);
            argv.push(std::ffi::OsString::from("mcp"));
            argv.extend(rest);
            std::process::exit(rhema_mcp::cli::run(&argv));
        }
        // The vision CLI owns the process: its exit code is ours, its
        // stderr is the model's error text.
        Some(crate::args::Command::Vision { args: rest }) => {
            let mut argv = Vec::with_capacity(rest.len() + 1);
            argv.push(std::ffi::OsString::from(rhema_agent_tools::vision::MODE));
            argv.extend(rest);
            std::process::exit(rhema_agent_tools::vision::cli_run(&argv));
        }
        // The stubs' road: `rhema __server …` and every unrecognized
        // subcommand go to the MCP CLI, whose exit code owns the
        // process. An unknown mode is its exit-2 usage error.
        Some(crate::args::Command::External(tokens)) => {
            std::process::exit(rhema_mcp::cli::run(&tokens));
        }
        Some(crate::args::Command::Run(run)) => return run_headless(run).await,
        Some(crate::args::Command::System { action }) => {
            return system_command(action, &args.workspace, args.profile.as_deref());
        }
        // The wizard is the terminal UI with a flag flipped; the
        // terminal check below still applies.
        Some(crate::args::Command::Onboard) => {}
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

/// The prompt's two optional sections for a workspace, the session's
/// sandbox kind, and the model it runs: the user's global `USER.md`
/// verbatim when present, and the environment report — host facts for a
/// local session, the honest notice for a sandboxed one, because host
/// facts would be lies about a container. `None` and `local` are the
/// local case; any other stored kind names the sandbox the notice
/// carries. The one gate: the agent's assembly and the `system show`
/// preview both read here, so what is previewed is what turns send.
pub(crate) fn prompt_sections(
    workspace: &std::path::Path,
    sandbox_workspace: &str,
    sandbox_kind: Option<&str>,
    model: &str,
    harness: &str,
) -> (Option<String>, Option<String>) {
    let user = Config::dir()
        .ok()
        .and_then(|dir| std::fs::read_to_string(dir.join("USER.md")).ok())
        .filter(|text| !text.trim().is_empty());
    let environment = match sandbox_kind {
        None | Some("local") => rhema_coding_agent::host_report(workspace, model, harness),
        Some(kind) => rhema_coding_agent::remote_notice(kind, sandbox_workspace, model, harness),
    };
    (Some(environment), user)
}

/// The process exit code for a failed run: 2 when the failure is the
/// model backend's — a rate limit, a dead endpoint, an auth rejection —
/// so a wrapper can tell a quota wall from a rhema bug and decide on its
/// own when to retry; 1 for everything else. Codes above 1 are shell
/// convention for distinct failures.
pub fn exit_code(err: &anyhow::Error) -> i32 {
    use rhema_coding_agent::CodingAgentError;
    for cause in err.chain() {
        if let Some(CodingAgentError::Agent(rhema_core::AgentError::Provider(_))) =
            cause.downcast_ref::<CodingAgentError>()
        {
            return 2;
        }
    }
    1
}

/// Runs the TUI: resolves credentials, sets up the terminal, and runs the
/// event loop until the app quits. The terminal is restored on every exit
/// path, including panics.
async fn run_tui(args: Args) -> anyhow::Result<()> {
    // Credential and session resolution happens BEFORE the terminal setup,
    // so early failures print as plain errors on a normal terminal.
    let store: Arc<dyn ConfigStore> = Arc::new(DiskStore);
    // The wizard runs when no config existed or the last wizard never
    // finished (`onboarded: Some(false)`), and when `rhema onboard` said
    // so. An existing config from before the wizard has no marker and
    // reads as onboarded — upgrading never replays it. The marker is
    // written up front so a wizard abandoned halfway (a key added, then
    // a ctrl-c) reopens here next start instead of reading as complete.
    let config_existed = Config::path().map(|path| path.exists()).unwrap_or(true);
    let mut config = store.load().unwrap_or_else(|err| {
        eprintln!("warning: ignoring unreadable rhema config: {err}");
        Config::default()
    });
    let onboard = !config_existed
        || config.onboarded == Some(false)
        || matches!(args.command, Some(crate::args::Command::Onboard));
    if onboard && config.onboarded != Some(false) {
        config.onboarded = Some(false);
        // A bundled skill that ships off starts in the disabled set —
        // the set is the only switch, so its default state must be
        // recorded, or seeding would read it as on.
        for skill in crate::bundled::BUNDLED_SKILLS {
            if !skill.default_on {
                config.skills_disabled.insert(skill.name.to_owned());
            }
        }
        let _ = config.save();
    }
    migrate_stored_keys(&mut config);
    // The bundled skills seed into the global directory before anything
    // discovers them — the wizard's skills step and the agent's prompt
    // both read the same folders. Best effort: a read-only config
    // directory only means fewer skills.
    if let Ok(dir) = Config::skills_dir()
        && let Err(err) = crate::bundled::seed(&dir, &config.skills_disabled)
    {
        eprintln!("warning: the bundled skills were not seeded: {err}");
    }
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
    // The file this conversation persists to: a resumed run continues
    // its own file, a fresh one gets a name from the sessions cache.
    // This is the checkpoint target every turn's streams commit into,
    // so an exit in the middle of the first turn — a crash, a kill, a
    // quit — still leaves something to resume. Allocation only picks
    // the name; nothing is written until a turn commits.
    let session_file = match &args.resume {
        Some(path) => Some(path.clone()),
        None => match sessions::base_dir() {
            Ok(base) => sessions::next_session_path(&base, &workspace)
                .map(Some)
                .unwrap_or_else(|err| {
                    eprintln!("warning: the session will only be saved on exit: {err}");
                    None
                }),
            Err(err) => {
                eprintln!("warning: the session will only be saved on exit: {err}");
                None
            }
        },
    };
    let agent_base = resolve_base_system(Config::system_prompt_path().ok().as_deref());
    let thinking = args
        .thinking
        .map(rhema_core::request::ThinkingEffort::from)
        .unwrap_or_else(|| config.effective(Some(&profile)).thinking_effort());
    // Where bash runs: the offered list is this machine's and this
    // profile's, the choice the same tiered resolution the picker
    // offers (`RHEMA_SANDBOX`/stored, else the workspace's remembered
    // kind, else local), and a non-local choice assembles — through the
    // headless path's shared `assemble` — before the first paint. A
    // failed assembly degrades to the host with a warning: a
    // remembered daemon that is down must not keep the UI from opening.
    let sandbox_machines = crate::sandbox::SandboxMachines::probe();
    let sandbox_offered = sandbox_machines.offered_for(&effective);
    let sandboxed = match startup_sandbox(&config, &effective, &workspace, &sandbox_offered) {
        crate::args::SandboxChoice::Local => None,
        non_local => {
            match crate::sandbox::assemble(
                non_local,
                &effective,
                &workspace,
                Some(&*secrets),
                agent_base.as_deref(),
            )
            .await
            {
                Ok(assembled) => Some(assembled),
                Err(err) => {
                    eprintln!(
                        "warning: the {} sandbox is unavailable ({err}); bash runs on this host",
                        non_local.stored_name()
                    );
                    None
                }
            }
        }
    };
    // The look and feel is a machine choice, not a per-invocation one:
    // the stored palette, theme, and icon flavor decide, unparseable
    // values read as the defaults.
    let colors = crate::palette::Palette::from_stored(config.colors.as_deref()).unwrap_or_default();
    let theme = match config.theme.as_deref() {
        Some("light") => crate::palette::ThemeKind::Light,
        _ => crate::palette::ThemeKind::Dark,
    };
    let m3_source = config
        .m3_source
        .as_deref()
        .and_then(crate::palette::parse_hex_u32);
    let custom = config.custom.clone();
    let icons = config
        .icons
        .as_deref()
        .and_then(crate::icons::Flavor::parse)
        .unwrap_or_default();
    let options = RunOptions {
        colors,
        theme,
        m3_source,
        custom,
        icons,
        profile: Some(profile),
        workspace: workspace.clone(),
        max_steps: args.max_steps,
        no_transcript: args.no_transcript,
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
            checkpoint: session_file,
            options,
            secrets: Some(Arc::clone(&secrets)),
            sandboxed,
            sandbox_machines,
            home: None,
            onboard,
        })?;
        let effects = app.initial_effects();
        (app, effects)
    };

    enable_raw_mode().context("the rhema-tui binary needs a terminal; pipe output through a pty or use the rhema CLI instead")?;
    // The hook goes on before anything else mutates the terminal: a
    // panic from here to the loop's first frame must not strand the
    // user's shell.
    install_panic_hook();
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
    // Every plain `?` exit between raw mode and the first frame also
    // has to restore: the shell must not keep raw mode on, or the
    // alternate screen. `restore_terminal` is idempotent and swallows
    // its own errors, so running it here is safe whatever landed.
    let setup = (|| -> anyhow::Result<Terminal<CrosstermBackend<std::io::Stdout>>> {
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
        Terminal::new(CrosstermBackend::new(stdout()))
            .context("cannot initialize the terminal backend")
    })();
    let mut terminal = match setup {
        Ok(terminal) => terminal,
        Err(error) => {
            restore_terminal();
            return Err(error);
        }
    };

    let (tx, mut rx) = unbounded_channel::<UiEvent>();
    let outcome = event_loop(&mut terminal, &mut app, &tx, &mut rx, &mut effects).await;

    restore_terminal();
    // Best-effort save for /resume, after the terminal is back: an empty
    // session saves nothing, and a failed save warns without failing the
    // exit. The save goes to the file the turns checkpointed, so one
    // conversation is one entry. A turn still out owns the agent — the
    // freshest state is the checkpoint file its stream commits wrote,
    // and writing the pre-turn snapshot the app still holds over it
    // would erase the interrupted turn, so nothing is exported here.
    if !app.turn.running
        && let Some((session, _)) = app.export_session()
    {
        let saved = match app.checkpoint_target() {
            Some(path) => sessions::write_session(&path, &session),
            None => sessions::base_dir()
                .and_then(|base| sessions::save_session(&base, &workspace, &session))
                .map(|_| ()),
        };
        if let Err(err) = saved {
            eprintln!("warning: the session was not saved for /resume: {err}");
        }
    }
    // Stop-on-exit: the last session out of a rhema-derived instance
    // stops it; a peer's shared hold says otherwise and the instance
    // stays up. The terminal is already back, so the stop's wait (a
    // container, a second; a vm, a few) lands on a plain shell prompt.
    if let Some(mut sandboxed) = app.sandboxed.take() {
        sandboxed.shutdown();
    }
    outcome
}

/// Restores the terminal: raw mode off, alternate screen left. Errors are
/// ignored so a restore attempt never panics (it runs in the panic hook).
fn restore_terminal() {
    let _ = crossterm::terminal::disable_raw_mode();
    // The onboarding logo goes before the screens swap: a terminal that
    // keeps alternate-screen images must not show one over the shell.
    crate::term_image::clear();
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

/// The UI thread's loop: draw, drain the event channel, run the app's
/// tick (subagent finishes and their notifications), poll crossterm.
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
    // The last logo rect the terminal graphics layer holds; the frame
    // below re-states its want every draw, and a change re-places the
    // image (a resize) or clears it (the wizard's screen went away).
    let mut logo_placed: Option<ratatui::layout::Rect> = None;
    for effect in std::mem::take(effects) {
        dispatch(terminal, app, tx, &mut status, effect);
    }
    loop {
        status.sync(app);
        terminal.draw(|frame| ui::render(app, frame))?;
        if app.logo_rect != logo_placed {
            match app.logo_rect {
                Some(rect) => crate::term_image::place(rect.x, rect.y, rect.width, rect.height),
                None => crate::term_image::clear(),
            }
            logo_placed = app.logo_rect;
        }

        while let Ok(event) = rx.try_recv() {
            status.observe(&event);
            for effect in app.on_event(event) {
                dispatch(terminal, app, tx, &mut status, effect);
            }
            if app.should_quit {
                return Ok(());
            }
        }
        // The tick runs after the drain: a turn that just finished
        // handed the agent back, so a notification that waited for the
        // idle state prompts now, not a redraw later.
        for effect in app.on_tick() {
            dispatch(terminal, app, tx, &mut status, effect);
        }
        if app.should_quit {
            return Ok(());
        }

        if crossterm::event::poll(POLL_INTERVAL)? {
            match crossterm::event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    for effect in app.on_key(key) {
                        dispatch(terminal, app, tx, &mut status, effect);
                    }
                }
                Event::Mouse(mouse) => match mouse.kind {
                    crossterm::event::MouseEventKind::ScrollUp => app.on_mouse_scroll(true),
                    crossterm::event::MouseEventKind::ScrollDown => app.on_mouse_scroll(false),
                    // A click is the button's release: press-drag-release
                    // lands once, on the row the user let go over.
                    crossterm::event::MouseEventKind::Up(crossterm::event::MouseButton::Left) => {
                        for effect in app.on_click(mouse.column, mouse.row) {
                            dispatch(terminal, app, tx, &mut status, effect);
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
fn dispatch(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    app: &mut App,
    tx: &UnboundedSender<UiEvent>,
    status: &mut Tracker,
    effect: Effect,
) {
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
        Effect::ResumeSession(path) => {
            spawn_session_resume(path, tx.clone());
        }
        Effect::FetchUsage => {
            // One allowance per session: the active backend's only, and
            // the fetch failing just keeps the badge off.
            let Some(backend) = app.backend.clone() else {
                return;
            };
            spawn_usage_fetch(backend, tx.clone());
        }
        Effect::StartTurn { text, images } => {
            let Some(agent) = app.agent.take() else {
                return;
            };
            let (cancel, cancelled) = tokio::sync::watch::channel(false);
            let (steer, steered) = tokio::sync::mpsc::channel(crate::app::STEER_QUEUE_CAP);
            // The status names the prompt until a tool call takes over.
            status.note_prompt(&text);
            // Auto-approved for now: the Approval seam — and the modal
            // wired through it — is where command allowlists will land.
            app.begin_turn(cancel, steer, None);
            spawn_turn(
                agent,
                crate::turn::Prompt::User { text, images },
                Arc::new(AllowAll),
                app.checkpoint_target(),
                cancelled,
                steered,
                tx.clone(),
            );
        }
        Effect::RetryTurn => {
            let Some(agent) = app.agent.take() else {
                return;
            };
            let (cancel, cancelled) = tokio::sync::watch::channel(false);
            let (steer, steered) = tokio::sync::mpsc::channel(crate::app::STEER_QUEUE_CAP);
            // The strip names the rerun until a tool call takes over.
            status.note_prompt("retrying");
            app.begin_turn(cancel, steer, None);
            spawn_turn(
                agent,
                crate::turn::Prompt::Retry,
                Arc::new(AllowAll),
                app.checkpoint_target(),
                cancelled,
                steered,
                tx.clone(),
            );
        }
        Effect::SteerTurn(text) => {
            // The app already sent the message over the turn's steer
            // channel; the strip names the interruption until the next
            // tool call takes over.
            status.note_prompt(&text);
        }
        Effect::StartLoginFlow => {
            let provider = match &app.screen {
                crate::app::Screen::Settings(settings) => settings
                    .dialog
                    .as_ref()
                    .map(|dialog| dialog.selected_provider()),
                crate::app::Screen::Onboarding(wizard) => wizard
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
        Effect::AssembleSandbox(choice) => {
            // The assembly reads the profile, the workspace, and the
            // base system as they stood at the pick; the keychain Arc
            // crosses because the task outlives the borrow.
            let profile = app.effective();
            let workspace = app.options.workspace.clone();
            let secrets = app.secrets.clone();
            let base_system = app.options.base_system.clone();
            crate::turn::spawn_sandbox_assemble(
                choice,
                profile,
                workspace,
                secrets,
                base_system,
                tx.clone(),
            );
        }
        Effect::EditSubagent(path) => {
            // Blocking here is the point: the editor owns the terminal,
            // so nothing of the app — draw, input, tasks — can run
            // until it is back. The app is on the settings screen, not
            // mid-turn; the runtime's background work stalls with it.
            if let Err(err) = edit_in_editor(terminal, &path) {
                app.transcript.push_notice(&format!("editor: {err}"));
            }
            app.refresh_subagents();
        }
    }
}

/// Suspends the TUI, runs `$EDITOR` on `path`, and re-enters: the
/// alternate screen and the input modes the editor cannot share are
/// dropped first and re-established after, mirroring `run_tui`'s
/// setup and `restore_terminal`'s teardown. The draw cache is
/// invalidated — the re-entered alternate screen is fresh, and a diff
/// against the old one would repaint half a screen. The editor's
/// error, if any, is the return.
fn edit_in_editor(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    path: &std::path::Path,
) -> anyhow::Result<()> {
    use std::io::Write as _;
    // The kitty flags pop before the screen they were pushed on is
    // left, and each step runs on its own so one failure cannot skip
    // the rest (the same order restore_terminal tears down in).
    let _ = crossterm::execute!(
        stdout(),
        PopKeyboardEnhancementFlags,
        DisableBracketedPaste,
        DisableMouseCapture,
        LeaveAlternateScreen
    );
    let _ = disable_raw_mode();
    let _ = stdout().flush();
    let result = run_editor(path, None);
    let _ = enable_raw_mode();
    let _ = crossterm::execute!(
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
    );
    let _ = stdout().flush();
    let _ = terminal.clear();
    result
}
