//! The runner: terminal setup and teardown, the event loop, and the
//! dispatch of the app's effects onto the runtime.
//!
//! TERMINAL OWNERSHIP: the UI thread is the only user of the terminal. The
//! turn and fetch tasks report through the event channel; they never
//! write. Raw mode + alternate screen are entered once here and restored
//! on EVERY exit path, after the event loop returns, and again in the
//! panic hook (restore is idempotent enough: failures are ignored), so a
//! panic never leaves the user inside the alternate screen. A watcher
//! thread repeats the restore for the fatal signals that unwind
//! nothing, the SIGHUP of an ssh drop or a closed window, SIGTERM,
//! SIGINT, and startup clears whatever a run without this teardown
//! left pushed, so one leaked state never compounds.

use std::io::{IsTerminal, stdout};
use std::sync::Arc;
use std::time::Duration;

use crate::store::Config;
use anyhow::Context;
use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture, Event,
    KeyEventKind,
};
#[cfg(unix)]
use crossterm::event::{KeyboardEnhancementFlags, PushKeyboardEnhancementFlags};
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use rhema_acp::AcpOptions;
use rhema_config::secrets::SecretStore;
use rhema_core::{Event as ModelEvent, EventSink, Session};
use rhema_host_agent::HostAgent;
use rhema_session_store_file::PathStore;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

use crate::app::Backend;
use crate::app::{App, AppSetup, Effect, RunOptions, startup_profile, startup_sandbox};
use crate::args::{Args, ProviderChoice, RunArgs, SystemCommand};
use crate::sessions;
use crate::status::{self, Status, Tracker, first_line};
use crate::store::{ConfigStore, DiskStore};
use crate::turn::{
    spawn_clipboard_grab, spawn_model_fetch, spawn_session_resume, spawn_turn, spawn_update_check,
    spawn_update_install, spawn_usage_fetch,
};
use crate::{events::UiEvent, ui};

mod headless;
mod system;
#[cfg(test)]
mod tests;
mod update;

use headless::run_headless;
pub(crate) use system::HARNESS;
use system::{run_editor, system_command};
use update::update_command;

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

/// Moves the config's stored literal keys into the encrypted file
/// store, best effort, rewriting the file so the move is recorded. A
/// failed move keeps its literal, retried here on every start.
fn migrate_stored_keys(config: &mut Config, secrets: Option<&dyn SecretStore>) {
    let Some(secrets) = secrets else {
        return;
    };
    let moved = config.migrate_secrets(secrets);
    if moved == 0 {
        return;
    }
    eprintln!("· moved {moved} provider key(s) into the encrypted secret store");
    if let Err(err) = config.save() {
        eprintln!("warning: the config file still names the moved keys: {err}");
    }
}

/// Runs the frontend: the ACP server, the `run` one-shot, or the
/// full-screen TUI, the subcommand decides, and bare `rhema` is the TUI
/// only when a terminal is attached.
pub async fn run(args: Args) -> anyhow::Result<()> {
    // The ACP branch first: stdin/stdout are the protocol wire, so no
    // terminal detection may run, a served agent never has a tty.
    match args.command {
        Some(crate::args::Command::Acp(acp)) => return run_acp(acp),
        Some(crate::args::Command::KitVersion) => {
            println!("{}", rhema_bash::runtime::KIT_VERSION);
            std::process::exit(0);
        }
        // The static init file: one call per kit install, printed for
        // the host to write beside the helper. The file carries no
        // session values; those are env.
        Some(crate::args::Command::Init { stub }) => {
            use rhema_bash::runtime::{Stub, stubs_text};
            // The helper's directory carries `gob` beside it; every
            // wired shell sees it on PATH.
            if let Some(dir) = std::env::current_exe()
                .ok()
                .and_then(|exe| exe.parent().map(std::path::Path::to_path_buf))
            {
                let dir = dir.to_string_lossy().replace('\'', "'\\''");
                println!("export PATH='{dir}':\"$PATH\"");
            }
            let mut stubs = Vec::with_capacity(stub.len());
            for raw in &stub {
                match Stub::parse(raw) {
                    Ok(parsed) => stubs.push(parsed),
                    Err(err) => {
                        eprintln!("{err}");
                        std::process::exit(2);
                    }
                }
            }
            print!("{}", stubs_text(&stubs));
            std::process::exit(0);
        }
        // The vision CLI owns the process: its exit code is ours, its
        // stderr is the model's error text.
        Some(crate::args::Command::Vision { args: rest }) => {
            let mut argv = Vec::with_capacity(rest.len() + 1);
            argv.push(std::ffi::OsString::from(rhema_bash::vision::MODE));
            argv.extend(rest);
            std::process::exit(rhema_bash::vision::cli_run(&argv));
        }
        // The jobs macro's road: `job …` in the agent shell execs
        // `rhema __jobs …`, whose exit code owns the process.
        Some(crate::args::Command::Jobs { args: rest }) => {
            let mut argv = Vec::with_capacity(rest.len() + 1);
            argv.push(std::ffi::OsString::from(rhema_bash::jobs::MODE));
            argv.extend(rest);
            std::process::exit(rhema_bash::jobs::cli_run(&argv));
        }
        // The search macro's road: `search "query"` in the agent
        // shell execs `rhema __search …`, whose exit code owns the
        // process and whose stderr is the model's error text.
        Some(crate::args::Command::Search { args: rest }) => {
            let mut argv = Vec::with_capacity(rest.len() + 1);
            argv.push(std::ffi::OsString::from(rhema_bash::websearch::MODE));
            argv.extend(rest);
            std::process::exit(crate::search_wire::cli_run(&argv));
        }
        // The decide macro's road: `decide …` in the agent shell execs
        // `rhema __decision …`, whose exit code owns the process and
        // whose stderr is the model's error text.
        Some(crate::args::Command::Decision { args: rest }) => {
            let mut argv = Vec::with_capacity(rest.len() + 1);
            argv.push(std::ffi::OsString::from(rhema_bash::decision::MODE));
            argv.extend(rest);
            std::process::exit(crate::decision_wire::cli_run(&argv));
        }
        // The update verb prints for a pipe as happily as for a
        // terminal, so it runs before the terminal check below.
        Some(crate::args::Command::External(tokens)) => {
            let name = tokens
                .first()
                .map(|token| token.to_string_lossy().into_owned())
                .unwrap_or_default();
            eprintln!("unknown subcommand `{name}`");
            std::process::exit(2);
        }
        Some(crate::args::Command::Update) => return update_command().await,
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
            "stdout is not a terminal: nothing to start interactively, \
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
/// verbatim when present, and the environment report, host facts for a
/// local session, the honest notice for a sandboxed one, because host
/// facts would be lies about a container. `None` and `local` are the
/// local case; any other stored kind names the sandbox the notice
/// carries. The third element is the probe's absence list, the user's
/// view of the same run: a local session names the tools it misses, a
/// sandboxed one claims none, the host's gaps are not a sandbox's.
/// The one gate: the agent's assembly and the `system show` preview
/// both read here, so what is previewed is what turns send.
pub(crate) fn prompt_sections(
    workspace: &std::path::Path,
    sandbox_workspace: &str,
    sandbox_kind: Option<&str>,
    model: &str,
    harness: &str,
) -> (Option<String>, Option<String>, Vec<String>) {
    let user = rhema_config::Home::discover()
        .ok()
        .and_then(|home| std::fs::read_to_string(home.config.join("USER.md")).ok())
        .filter(|text| !text.trim().is_empty());
    let (environment, missing) = match sandbox_kind {
        None | Some("local") => {
            let host = rhema_host_agent::host_environment(workspace, model, harness);
            (host.report, host.missing)
        }
        Some(kind) => (
            rhema_host_agent::remote_notice(kind, sandbox_workspace, model, harness),
            Vec::new(),
        ),
    };
    (Some(environment), user, missing)
}

/// The process exit code for a failed run: 2 when the failure is the
/// model backend's, a rate limit, a dead endpoint, an auth rejection,
/// so a wrapper can tell a quota wall from a rhema bug and decide on its
/// own when to retry; 1 for everything else. Codes above 1 are shell
/// convention for distinct failures.
pub fn exit_code(err: &anyhow::Error) -> i32 {
    use rhema_host_agent::HostAgentError;
    for cause in err.chain() {
        if let Some(HostAgentError::Agent(rhema_core::AgentError::Provider(_))) =
            cause.downcast_ref::<HostAgentError>()
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
    // A finished update parked the image it ran from beside the new
    // one, and windows could not delete it while that process held it.
    // This start replaced it, so the leftover goes now.
    crate::update::clear_stale_install();
    // Credential and session resolution happens BEFORE the terminal setup,
    // so early failures print as plain errors on a normal terminal.
    let store: Arc<dyn ConfigStore> = Arc::new(DiskStore);
    // One home for the whole run: the config path, the skills folder,
    // and the secret store all come from it.
    let home = rhema_config::Home::discover()?;
    // The wizard runs when no config existed or the last wizard never
    // finished (`onboarded: Some(false)`), and when `rhema onboard` said
    // so. An existing config from before the wizard has no marker and
    // reads as onboarded, upgrading never replays it. The marker is
    // written up front so a wizard abandoned halfway (a key added, then
    // a ctrl-c) reopens here next start instead of reading as complete.
    let config_existed = home.config_file().exists();
    let (mut config, config_loaded) = match store.load() {
        Ok(config) => (config, true),
        Err(err) => {
            eprintln!("warning: ignoring unreadable rhema config: {err}");
            (Config::default(), false)
        }
    };
    let onboard = !config_existed
        || config.onboarded == Some(false)
        || matches!(args.command, Some(crate::args::Command::Onboard));
    if onboard && config.onboarded != Some(false) {
        // The marker is written only on a config that loaded: this
        // run's document is defaults when it did not, and the write
        // would already be the destruction the guard exists for.
        // The wizard still opens below; its own save refuses the
        // same way.
        if !config_loaded {
            eprintln!(
                "warning: the onboarding marker was not written: {}",
                crate::store::UNLOADED_SAVE_REFUSAL
            );
        } else {
            config.onboarded = Some(false);
            // A bundled skill that ships off starts in the disabled set,
            // the set is the only switch, so its default state must be
            // recorded, or seeding would read it as on.
            for skill in crate::bundled::BUNDLED_SKILLS {
                if !skill.default_on {
                    config.skills_disabled.insert(skill.name.to_owned());
                }
            }
            let _ = config.save();
        }
    }
    let secrets: Arc<dyn SecretStore> = home.secret_store();
    migrate_stored_keys(&mut config, Some(secrets.as_ref()));
    // The old OS keyring is read once, here, for refs the encrypted
    // store is missing. The reader is one-way and stays out of an SSH
    // session unless RHEMA_IMPORT_KEYRING says otherwise.
    let imported = config.import_legacy_secrets(
        &rhema_config::secrets::KeyringSecretReader::for_home(&home),
        secrets.as_ref(),
    );
    if imported > 0 {
        eprintln!("· copied {imported} key(s) from the OS keyring into the encrypted store");
    }
    // The bundled skills seed into the global directory before anything
    // discovers them, the wizard's skills step and the agent's prompt
    // both read the same folders. Best effort: a read-only config
    // directory only means fewer skills.
    if let Err(err) = crate::bundled::seed(&home.skills(), &config.skills_disabled) {
        eprintln!("warning: the bundled skills were not seeded: {err}");
    }
    // A backend that is installed but too old would advertise `job` and
    // fail on the first call: say so once, here, while stderr is still a
    // plain terminal. An absent backend is silent by design.
    if let Some(problem) = rhema_bash::jobs::backend_problem() {
        eprintln!("warning: background jobs disabled: {problem}");
    }
    // The session's secret store: startup resolution reads it, and the
    // app places fresh logins in it and cleans up after removals.
    // Without an explicit --profile the session starts on the profile
    // this workspace last used, or on the default/first one that can
    // serve turns, a logged-in profile never opens the login screen.
    // The stored provider choice then decides which backend runs; a
    // missing key still leaves the backend unresolved so the login
    // screen runs.
    // The real path, before anything moves into the app: the profile
    // memory and the saved sessions group by it, so a relative "." must
    // not become a "." directory.
    let workspace = crate::args::canonical(&args.workspace);
    let profile = args
        .profile
        .clone()
        .unwrap_or_else(|| startup_profile(&config, &workspace, Some(&*secrets)));
    let effective = config.effective(Some(&profile));
    // RHEMA_PROVIDER (inside provider_name) wins over the stored
    // profile, the same env-wins convention the keys follow. Nothing
    // configured (or a spelling no provider answers to) leaves the
    // backend unresolved: the login screen opens, nothing defaults.
    let backend =
        ProviderChoice::from_stored(effective.provider_name().as_deref()).and_then(
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
                            api: None,
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
                ProviderChoice::OpenCode => {
                    effective
                        .api_key("opencode", Some(&*secrets))
                        .map(|api_key| {
                            Backend::opencode(
                                api_key,
                                effective
                                    .instance_for_provider("opencode")
                                    .and_then(|(_, instance)| effective.instance_url(instance)),
                            )
                        })
                }
                ProviderChoice::Copilot => effective
                    .api_key("copilot", Some(&*secrets))
                    .map(|api_key| Backend::Copilot { api_key }),
                ProviderChoice::ChatGpt => effective
                    .api_key("chatgpt", Some(&*secrets))
                    .map(|credential| Backend::ChatGpt { credential }),
                ProviderChoice::Mistral => {
                    effective
                        .api_key("mistral", Some(&*secrets))
                        .map(|api_key| Backend::Mistral {
                            api_key,
                            base_url: effective
                                .instance_for_provider("mistral")
                                .and_then(|(_, instance)| effective.instance_url(instance)),
                        })
                }
                ProviderChoice::MistralVibe => effective
                    .api_key("mistral-vibe", Some(&*secrets))
                    .map(|api_key| Backend::MistralVibe {
                        api_key,
                        base_url: effective
                            .instance_for_provider("mistral-vibe")
                            .and_then(|(_, instance)| effective.instance_url(instance)),
                    }),
            },
        );
    let session = match &args.resume {
        Some(path) => HostAgent::load_session_from(path)
            .with_context(|| format!("cannot resume session: {}", path.display()))?,
        None => Session::default(),
    };
    // The file this conversation persists to: a resumed run continues
    // its own file, a fresh one gets a name from the sessions cache.
    // This is the checkpoint target every turn's streams commit into,
    // so an exit in the middle of the first turn, a crash, a kill, a
    // quit, still leaves something to resume. Allocation only picks
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
    let agent_base = resolve_base_system(
        rhema_config::Home::discover()
            .ok()
            .map(|home| home.system_prompt())
            .as_deref(),
    );
    let thinking = args
        .thinking
        .map(rhema_core::request::ThinkingEffort::from)
        .unwrap_or_else(|| config.effective(Some(&profile)).thinking_effort());
    // Where bash runs: the offered list is this machine's and this
    // profile's, the choice the same tiered resolution the picker
    // offers (`RHEMA_SANDBOX`/stored, else the workspace's remembered
    // kind, else local), and a non-local choice assembles, through the
    // headless path's shared `assemble`, before the first paint. A
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
    let background = crate::palette::Background::parse(config.background.as_deref());
    let cursor =
        crate::cursor::CursorStyle::from_stored(config.cursor.as_deref()).unwrap_or_default();
    let scrollbar = config.scrollbar.unwrap_or(true);
    let options = RunOptions {
        colors,
        theme,
        background,
        m3_source,
        custom,
        icons,
        cursor,
        scrollbar,
        profile: Some(profile),
        workspace: workspace.clone(),
        max_steps: args.max_steps,
        no_transcript: args.no_transcript,
        thinking,
        base_system: agent_base,
        save: args.save,
    };
    // A config that failed to load poisons every save downstream, not
    // just this startup's: the wizard's finish, a settings pick, a
    // profile change all persist this run's document, and that
    // document is defaults. The guard store refuses each with the
    // reason, so the file survives until the user fixes it.
    let store: Arc<dyn ConfigStore> = if config_loaded {
        store
    } else {
        Arc::new(crate::store::UnloadedGuardStore::new(store))
    };
    let (mut app, mut effects) = {
        let mut app = App::new(AppSetup {
            store,
            config,
            backend,
            session,
            checkpoint: session_file,
            sessions_base: None,
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
    // The graphics flags ride the app so the renderer stays
    // environment-blind: the logo asks only for the protocol, the
    // card asks for the text layer too.
    app.graphics = crate::term_image::graphics();
    app.card_graphics =
        app.graphics && app.theme.prompt_fill_rgb().is_some() && crate::term_image::card_graphics();

    enable_raw_mode().context("the rhema-tui binary needs a terminal; pipe output through a pty or use the rhema CLI instead")?;
    // The hook goes on before anything else mutates the terminal: a
    // panic from here to the loop's first frame must not strand the
    // user's shell.
    install_panic_hook();
    // The panic hook cannot see a signal death: no unwinding, no
    // hook, just the default disposition tearing the process down
    // with every pushed mode still live in the emulator.
    #[cfg(unix)]
    install_signal_restore();
    // The window title: the mark, the workspace folder as the footer
    // spells it. `CSI 22;0t` saves the terminal's previous title on its
    // title stack (xterm, kitty, WezTerm keep one), and the matching pop
    // in `restore_terminal` hands it back on exit; a terminal without a
    // stack ignores both and keeps the rhema title until the shell's
    // next prompt overwrites it. The set itself rides OSC 0, the
    // spelling every terminal takes. Decoration, not function: write
    // failures are ignored, and a piped stdout takes no sequence at all.
    {
        use std::io::Write;
        let mut out = stdout();
        if out.is_terminal() {
            let _ = write!(
                out,
                "\x1b[22;0t\x1b]0;{}\x07",
                crate::ui::footer::window_title(&app)
            );
            let _ = out.flush();
        }
    }
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
    // key, `@` on AZERTY (AltGr+0) arrives as `à` plus Ctrl+Alt. Only
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
    // A run that died without its teardown, a SIGKILL, a crash from
    // before the signal watcher, another program's leak, may have
    // left capture modes on; clear them before this run pushes its
    // own, or the new state sits on top of the old leak and the
    // shell's next exit is broken twice over.
    heal_terminal();
    let setup = (|| -> anyhow::Result<Terminal<CrosstermBackend<std::io::Stdout>>> {
        crossterm::execute!(stdout(), EnterAlternateScreen)
            .context("cannot enter the alternate screen")?;
        // The kitty protocol is a unix terminal's answer; the windows
        // console has no progressive enhancement to ask for (crossterm
        // refuses the push there), and its legacy key records already
        // carry arrows, modifiers, and shift+enter.
        #[cfg(unix)]
        crossterm::execute!(
            stdout(),
            PushKeyboardEnhancementFlags(
                KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                    | KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS
                    | KeyboardEnhancementFlags::REPORT_ALL_KEYS_AS_ESCAPE_CODES
                    | KeyboardEnhancementFlags::REPORT_ASSOCIATED_TEXT
            ),
            EnableMouseCapture,
            EnableBracketedPaste
        )?;
        #[cfg(not(unix))]
        crossterm::execute!(stdout(), EnableMouseCapture, EnableBracketedPaste)?;
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
    app.options.cursor.apply();

    let (tx, mut rx) = unbounded_channel::<UiEvent>();
    let outcome = event_loop(&mut terminal, &mut app, &tx, &mut rx, &mut effects).await;

    restore_terminal();
    // Best-effort save for /resume, after the terminal is back: an empty
    // session saves nothing, and a failed save warns without failing the
    // exit. The save goes to the file the turns checkpointed, so one
    // conversation is one entry. A turn still out owns the agent, the
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

/// The kitty keyboard-protocol teardown every restore shares: the bare
/// `CSI < u` pops (crossterm's command emits the numbered `CSI <1u`,
/// which a terminal may ignore), and the trailing push of zero flags
/// masks any entry the pops could not remove, the effective encoding
/// lands on legacy either way. Each step writes on its own so one
/// failure cannot skip the rest. Callers run it on the alternate
/// screen, where the push happened.
fn pop_kitty_flags() {
    use std::io::Write;
    let _ = write!(stdout(), "\x1b[<u");
    let _ = stdout().flush();
    let _ = write!(stdout(), "\x1b[<u");
    let _ = stdout().flush();
    let _ = write!(stdout(), "\x1b[>0u");
    let _ = stdout().flush();
}

/// Drops the paste and mouse captures: two modes no sub-process (an
/// editor, a leaked previous run) can use.
fn drop_capture_modes() {
    use std::io::Write as _;
    let _ = crossterm::execute!(stdout(), DisableBracketedPaste, DisableMouseCapture);
    let _ = stdout().flush();
}

/// Leaves the alternate screen, capture modes first: the same escape
/// order every full teardown spells.
fn leave_screen() {
    use std::io::Write as _;
    drop_capture_modes();
    let _ = crossterm::execute!(stdout(), LeaveAlternateScreen);
    let _ = stdout().flush();
}

/// Restores the terminal: raw mode off, alternate screen left. Errors are
/// ignored so a restore attempt never panics (it runs in the panic hook).
fn restore_terminal() {
    let _ = crossterm::terminal::disable_raw_mode();
    // The onboarding logo goes before the screens swap: a terminal that
    // keeps alternate-screen images must not show one over the shell.
    crate::term_image::clear();
    pop_kitty_flags();
    // Only a session that wrote DECSCUSR puts the cursor back: a
    // default session never did, so the shell's own shape (a line under
    // shell integration) stays exactly as it was.
    crate::cursor::CursorStyle::restore();
    leave_screen();
    let _ = stdout().flush();
    // The program-status record goes with the session: an exited
    // interactive rhema leaves no badge behind (terminals that already
    // dropped the record on exit ignore this).
    status::clear();
    // The title stack pops with it: the pre-rhema title comes back on
    // the terminals that keep one (the push in run_tui paired this), a
    // terminal without one ignores the pop and leaves the title for the
    // shell to overwrite.
    use std::io::Write as _;
    let _ = write!(stdout(), "\x1b[23;0t");
    let _ = stdout().flush();
}

/// Watches the fatal signals that bypass both the event loop and the
/// panic hook, the SIGHUP of an ssh drop or a closed terminal window,
/// SIGTERM, SIGINT, and restores the terminal before dying with the
/// shell's `128 + signal` code. Without it a dropped connection leaves
/// the emulator holding every mode the run pushed: a captured mouse
/// that pours `^[[<…M` motion reports into the next shell, raw mode,
/// the kitty keyboard protocol, bracketed paste.
#[cfg(unix)]
fn install_signal_restore() {
    use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM};
    use signal_hook::iterator::Signals;
    // A registration failure leaves the default dispositions and the
    // pre-watcher behavior; not a reason to abort a start over.
    let Ok(mut signals) = Signals::new([SIGHUP, SIGTERM, SIGINT]) else {
        return;
    };
    // A plain OS thread on purpose: a wedged runtime is exactly the
    // abrupt-death case this exists for, so the restore must not
    // depend on tokio or the event loop being alive. A failed spawn
    // degrades the same way a failed registration does.
    let _ = std::thread::Builder::new()
        .name("signal-restore".into())
        .spawn(move || {
            // One wait for the process's lifetime: whichever of the
            // watched signals lands first (`forever` never ends, so
            // this `next` blocks until it does). The shell convention
            // reports a signal death as 128 + the signal, so a
            // wrapper can tell it from a normal exit.
            if let Some(signal) = signals.forever().next() {
                restore_terminal();
                std::process::exit(128 + signal);
            }
        });
}

/// Clears the pushed modes a previous run may have leaked: one that
/// died without its restore leaves mouse capture, bracketed paste,
/// and the kitty keyboard protocol live in the emulator, and the
/// shell behind it drowns in `^[[<…M` motion reports. Every disable
/// here is a no-op when nothing leaked, and this run re-pushes the
/// modes it needs right after, this only stops a leak from
/// compounding into a second broken run.
fn heal_terminal() {
    use std::io::Write as _;
    // The same spellings restore_terminal tears down with: the bare
    // pops and the zero-flag mask, then the two capture modes. No
    // alternate-screen leave: this runs before the screens swap.
    pop_kitty_flags();
    drop_capture_modes();
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

/// The UI thread's loop: draw, drain the event channel, run the app's
/// tick (subagent finishes and their notifications), poll crossterm.
/// Nothing here ever blocks on a task, that is the approval bridge's
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
    // The last card the graphics layer holds, and the fill the sprite
    // ids carry: the frame below re-states its want every draw, and a
    // change moves the placements or clears them. Each piece owns a
    // stable placement slot, and restating a slot moves and resizes
    // it, the protocol replaces the pair in place, no flicker and no
    // delete, so a scrolled transcript writes placements only. A
    // fill change alone re-transmits the five sprites under the same
    // ids, and the re-transmit drops the layer's placements, which
    // the same frame's re-states rebuild.
    let mut pieces_placed: Vec<crate::ui::BubblePiece> = Vec::new();
    let mut sprite_fills = (None, None);
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
            // The logo's escapes delete every image on the terminal,
            // the card's sprites included: the next bubble re-transmits.
            sprite_fills = (None, None);
            pieces_placed.clear();
        }
        // The cards re-state their want like the logo: the frame's
        // glyphs and text draw above them (the placements carry z=-1,
        // under the text layer), and the cells repaint around them.
        // The sprites transmit once a fill color; a scrolled
        // transcript moves and resizes only placements, in place, and
        // a piece that leaves the viewport drops its own slot, never
        // the global delete, whose semantics a terminal may widen past
        // the spec and take the sprites' data with it. Two fills, two
        // sets: the prompt's accent card and the notification's
        // neutral one, each nine sprites under its own ids.
        let fills = (app.theme.prompt_fill_rgb(), app.theme.notice_fill_rgb());
        if app.bubble_pieces != pieces_placed
            || (!app.bubble_pieces.is_empty() && fills != sprite_fills)
        {
            if app.bubble_pieces.is_empty() {
                for piece in pieces_placed.drain(..) {
                    crate::term_image::delete_placement(piece.sprite, piece.slot);
                }
            } else if let (Some(prompt_fill), Some(notice_fill)) = fills {
                // The sprites retransmit at the head of every burst,
                // not once per fill: a retransmit swaps the pixels and
                // leaves the placements keyed on the id alone, so it
                // costs a few kilobytes and heals any state a terminal
                // lost, ghostty before 7f62fe70a frees an image's
                // data on a placement delete that matched nothing,
                // after which every placement of it failed with a
                // silent ENOENT (the case of the sheared bottom
                // corners). A transmit per burst makes that
                // unobservable.
                for (offset, fill) in [
                    (0, prompt_fill),
                    (crate::term_image::NOTICE_SPRITE_OFFSET, notice_fill),
                ] {
                    for corner in crate::term_image::Corner::ALL {
                        crate::term_image::transmit_sprite(
                            corner.sprite_id() + offset,
                            &crate::term_image::corner_sprite(corner, fill),
                        );
                    }
                    for edge in crate::term_image::Edge::ALL {
                        crate::term_image::transmit_sprite(
                            edge.sprite_id() + offset,
                            &crate::term_image::edge_sprite(edge, fill),
                        );
                    }
                }
                sprite_fills = fills;
                for piece in &app.bubble_pieces {
                    crate::term_image::place_piece(
                        piece.sprite,
                        piece.slot,
                        piece.col,
                        piece.row,
                        piece.cols,
                        piece.rows,
                    );
                }
                // The placements the frame gave up, gone one at a
                // time, the images staying transmitted.
                for piece in stale_placements(&pieces_placed, &app.bubble_pieces) {
                    crate::term_image::delete_placement(piece.sprite, piece.slot);
                }
            }
            pieces_placed.clone_from(&app.bubble_pieces);
            // The placements homed the terminal cursor onto their
            // anchors; the input's cursor goes back where the frame
            // put it. A frame without a cursor hid it, and a moved
            // hidden cursor is nothing to restore.
            if let Some(anchor) = app.cursor_anchor {
                let _ = terminal.set_cursor_position(anchor);
            }
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
                    // A left drag over the transcript is a text
                    // selection; the release copies it. A release with
                    // no selection to consume is a click, and lands once,
                    // on the row the user let go over: press-drag-release
                    // still routes through one `on_click`.
                    crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left) => {
                        app.on_mouse_down(mouse.column, mouse.row)
                    }
                    crossterm::event::MouseEventKind::Drag(crossterm::event::MouseButton::Left) => {
                        app.on_mouse_drag(mouse.column, mouse.row)
                    }
                    crossterm::event::MouseEventKind::Up(crossterm::event::MouseButton::Left) => {
                        let selected = app.finish_selection(mouse.column, mouse.row);
                        if !selected {
                            for effect in app.on_click(mouse.column, mouse.row) {
                                dispatch(terminal, app, tx, &mut status, effect);
                            }
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

/// The placements the frame's card no longer holds: the slots that
/// dropped, and, just as stale, the slots that changed sprite. A
/// placement is the pair of the image id and the slot; a slot that
/// renumbers onto a different sprite (a bubble scrolled out and the
/// next bubble's piece inherited its slot) would leave the old pair
/// on screen forever if only the slot were compared, the ghost
/// corners a scroll used to trail behind the card.
fn stale_placements(
    placed: &[crate::ui::BubblePiece],
    current: &[crate::ui::BubblePiece],
) -> Vec<crate::ui::BubblePiece> {
    let held: Vec<(u16, u32)> = current
        .iter()
        .map(|piece| (piece.slot, piece.sprite))
        .collect();
    placed
        .iter()
        .filter(|piece| {
            held.iter()
                .any(|(slot, sprite)| slot == &piece.slot && sprite != &piece.sprite)
                || !held.iter().any(|(slot, _)| slot == &piece.slot)
        })
        .copied()
        .collect()
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
            // A new batch starts from nothing: the names and the error of
            // the last one are settled facts, not current ones, and this
            // batch's own answers reset them one provider at a time.
            if !matches!(why, crate::events::FetchWhy::Login { .. }) {
                app.catalog_unavailable.clear();
                app.catalog_failed.clear();
                app.catalog_error = None;
                app.catalog_inflight = true;
            }
            spawn_model_fetch(backends, why, tx.clone());
        }
        Effect::GrabClipboard => {
            spawn_clipboard_grab(tx.clone());
        }
        Effect::ResumeSession(path) => {
            spawn_session_resume(path, tx.clone());
        }
        Effect::CheckForUpdates => {
            spawn_update_check(tx.clone());
        }
        Effect::InstallUpdate => {
            spawn_update_install(tx.clone());
        }
        Effect::FetchUsage => {
            // One allowance per session: the active backend's only, and
            // the fetch failing just keeps the badge off.
            let Some(backend) = app.backend.clone() else {
                return;
            };
            spawn_usage_fetch(backend, tx.clone());
        }
        Effect::StartTurn {
            text,
            images,
            spans,
        } => {
            let Some(mut agent) = app.agent.take() else {
                return;
            };
            let (cancel, cancelled) = tokio::sync::watch::channel(false);
            let (steer, steered) = tokio::sync::mpsc::channel(crate::app::STEER_QUEUE_CAP);
            // The status names the prompt until a tool call takes over.
            status.note_prompt(&text);
            app.begin_turn(cancel, steer, None);
            let checkpoint = app.ensure_checkpoint_target();
            let submission = app.begin_turn_submission(&mut agent, &text);
            spawn_turn(
                agent,
                crate::turn::Prompt::User {
                    text,
                    images,
                    spans,
                },
                checkpoint,
                submission,
                cancelled,
                steered,
                tx.clone(),
            );
        }
        Effect::RetryTurn => {
            let Some(mut agent) = app.agent.take() else {
                return;
            };
            let (cancel, cancelled) = tokio::sync::watch::channel(false);
            let (steer, steered) = tokio::sync::mpsc::channel(crate::app::STEER_QUEUE_CAP);
            // The strip names the rerun until a tool call takes over.
            status.note_prompt("retrying");
            app.begin_turn(cancel, steer, None);
            let checkpoint = app.ensure_checkpoint_target();
            let submission = app.retry_turn_submission(&mut agent);
            spawn_turn(
                agent,
                crate::turn::Prompt::Retry,
                checkpoint,
                submission,
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
            // only builds the flow. Copilot's device grant and
            // ChatGPT's browser sign-in are the users; further flow
            // providers join behind the same arm.
            let flow: Box<dyn rhema_provider::InteractiveLogin> = match provider {
                Some(crate::screens::LoginProvider::Copilot) => {
                    Box::new(rhema_provider_copilot::CopilotLogin::new())
                }
                Some(crate::screens::LoginProvider::ChatGpt) => {
                    Box::new(rhema_provider_chatgpt::ChatGptLogin::new())
                }
                Some(crate::screens::LoginProvider::MistralVibe) => {
                    Box::new(rhema_provider_mistral::MistralVibeLogin::new())
                }
                _ => return,
            };
            let (cancel, cancelled) = tokio::sync::watch::channel(false);
            app.login_cancel = Some(cancel);
            crate::turn::spawn_login_flow(flow, cancelled, tx.clone());
        }
        Effect::AssembleSandbox(choice) => {
            // The assembly reads the profile, the workspace, and the
            // base system as they stood at the pick; the secret store Arc
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
        Effect::SetCursorStyle(style) => style.apply(),
        Effect::EditSubagent(path) => {
            // Blocking here is the point: the editor owns the terminal,
            // so nothing of the app, draw, input, tasks, can run
            // until it is back. The app is on the settings screen, not
            // mid-turn; the runtime's background work stalls with it.
            if let Err(err) = edit_in_editor(terminal, &path, app.options.cursor) {
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
/// invalidated, the re-entered alternate screen is fresh, and a diff
/// against the old one would repaint half a screen. The editor's
/// error, if any, is the return.
fn edit_in_editor(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    path: &std::path::Path,
    cursor: crate::cursor::CursorStyle,
) -> anyhow::Result<()> {
    // The teardown is restore_terminal's own order and spellings —
    // raw mode off, the bare kitty pops while still on the alternate
    // screen they were pushed on, the captures dropped, the screen
    // left — minus the exit-only steps (logo clear, cursor restore,
    // status record, title) that belong to a run's end.
    use std::io::Write as _;
    let _ = disable_raw_mode();
    pop_kitty_flags();
    leave_screen();
    let _ = stdout().flush();
    let result = run_editor(path, None);
    let _ = enable_raw_mode();
    let _ = crossterm::execute!(stdout(), EnterAlternateScreen);
    // The push mirrors the setup's: a unix terminal protocol only.
    #[cfg(unix)]
    let _ = crossterm::execute!(
        stdout(),
        PushKeyboardEnhancementFlags(
            KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                | KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS
                | KeyboardEnhancementFlags::REPORT_ALL_KEYS_AS_ESCAPE_CODES
                | KeyboardEnhancementFlags::REPORT_ASSOCIATED_TEXT
        ),
        EnableMouseCapture,
        EnableBracketedPaste
    );
    #[cfg(not(unix))]
    let _ = crossterm::execute!(stdout(), EnableMouseCapture, EnableBracketedPaste);
    let _ = stdout().flush();
    // An editor may leave its own caret shape behind; an explicit
    // session choice goes back the moment rhema owns the terminal again.
    cursor.apply();
    let _ = terminal.clear();
    result
}
