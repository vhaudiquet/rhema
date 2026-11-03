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

/// The `rhema system` utilities: they shape what turns would send and
/// run no model — no provider, profile, or credential is touched.
fn system_command(
    action: SystemCommand,
    workspace: &std::path::Path,
    profile: Option<&str>,
) -> anyhow::Result<()> {
    match action {
        SystemCommand::Edit => system_edit_at(&Config::system_prompt_path()?, None),
        SystemCommand::Restore => system_restore_at(&Config::system_prompt_path()?),
        SystemCommand::Show => {
            let prompt = system_show_at(
                Config::system_prompt_path().ok().as_deref(),
                workspace,
                profile,
            )?;
            // Styled markdown on a terminal; the byte-exact prompt when
            // piped, the same contract the one-shot keeps.
            if stdout().is_terminal() {
                let config = Config::load().unwrap_or_default();
                let theme = theme_from_config(&config);
                let width = crossterm::terminal::size()
                    .map(|(columns, _)| columns as usize)
                    .unwrap_or(80);
                print!("{}", markdown_ansi(&prompt, &theme, width));
            } else {
                print!("{prompt}");
            }
            Ok(())
        }
    }
}

/// The harness name the environment report carries: the frontend the
/// session runs in, with its version.
pub(crate) const HARNESS: &str = concat!("rhema ", env!("CARGO_PKG_VERSION"));

/// The stored look's theme — the same resolution the TUI makes, for the
/// one off-TUI surface that prints styled markdown.
fn theme_from_config(config: &Config) -> crate::palette::Theme {
    let colors = crate::palette::Palette::from_stored(config.colors.as_deref()).unwrap_or_default();
    let kind = match config.theme.as_deref() {
        Some("light") => crate::palette::ThemeKind::Light,
        _ => crate::palette::ThemeKind::Dark,
    };
    colors.theme(
        kind,
        crate::palette::PaletteInputs {
            m3_source: config
                .m3_source
                .as_deref()
                .and_then(crate::palette::parse_hex_u32),
            custom: config.custom.as_ref(),
        },
    )
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

/// Renders markdown to styled ANSI text through the TUI's own renderer,
/// so `system show` on a terminal reads like the transcript it previews.
/// One reset per span keeps a stray style from bleeding past its line.
fn markdown_ansi(source: &str, theme: &crate::palette::Theme, width: usize) -> String {
    use ratatui::style::Modifier;
    let mut out = String::new();
    for line in crate::markdown::render_markdown(source, theme, width) {
        for span in line.spans {
            let style = span.style;
            if let Some(color) = style.fg {
                out.push_str(
                    &crossterm::style::SetForegroundColor(crossterm_color(color)).to_string(),
                );
            }
            for modifier in [
                (Modifier::BOLD, crossterm::style::Attribute::Bold),
                (Modifier::ITALIC, crossterm::style::Attribute::Italic),
                (
                    Modifier::CROSSED_OUT,
                    crossterm::style::Attribute::CrossedOut,
                ),
                (
                    Modifier::UNDERLINED,
                    crossterm::style::Attribute::Underlined,
                ),
            ] {
                if style.add_modifier.contains(modifier.0) {
                    out.push_str(&crossterm::style::SetAttribute(modifier.1).to_string());
                }
            }
            out.push_str(span.content.as_ref());
            if style.fg.is_some() || !style.add_modifier.is_empty() {
                out.push_str(&crossterm::style::ResetColor.to_string());
            }
        }
        out.push('\n');
    }
    out
}

/// Maps a ratatui color onto the crossterm one of the same terminal
/// slot — the two crates disagree on which half of each pair is the
/// plain name, so the mapping is spelled out.
fn crossterm_color(color: ratatui::style::Color) -> crossterm::style::Color {
    use crossterm::style::Color as Ansi;
    use ratatui::style::Color as Ui;
    match color {
        Ui::Reset => Ansi::Reset,
        Ui::Black => Ansi::Black,
        Ui::Red => Ansi::DarkRed,
        Ui::Green => Ansi::DarkGreen,
        Ui::Yellow => Ansi::DarkYellow,
        Ui::Blue => Ansi::DarkBlue,
        Ui::Magenta => Ansi::DarkMagenta,
        Ui::Cyan => Ansi::DarkCyan,
        Ui::Gray => Ansi::Grey,
        Ui::DarkGray => Ansi::DarkGrey,
        Ui::LightRed => Ansi::Red,
        Ui::LightGreen => Ansi::Green,
        Ui::LightYellow => Ansi::Yellow,
        Ui::LightBlue => Ansi::Blue,
        Ui::LightMagenta => Ansi::Magenta,
        Ui::LightCyan => Ansi::Cyan,
        Ui::White => Ansi::White,
        Ui::Rgb(r, g, b) => Ansi::Rgb { r, g, b },
        Ui::Indexed(i) => Ansi::AnsiValue(i),
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
/// skills, the tools, and the two optional sections. The sandbox kind
/// resolves from the stored profile the way startup does, so the
/// preview and the first turn agree on host facts versus the notice.
fn system_show_at(
    path: Option<&std::path::Path>,
    workspace: &std::path::Path,
    profile: Option<&str>,
) -> anyhow::Result<String> {
    let workspace = std::fs::canonicalize(workspace).unwrap_or_else(|_| workspace.to_path_buf());
    let mut extra = rhema_subagents::extra_specs(&workspace);
    // The transcript is on by default, so the default preview names
    // zoom, exactly as the agent's prompt does.
    extra.push(rhema_transcript::Zoom::spec());
    let agents = Config::agents_skills_dir().ok();
    let global = Config::skills_dir().ok();
    let disabled = Config::load()
        .ok()
        .map(|c| c.skills_disabled)
        .unwrap_or_default();
    let mut skills =
        rhema_agent_tools::skills::discover(agents.as_deref(), global.as_deref(), &workspace);
    // The preview shows what turns send: a disabled skill is not in the
    // prompt, so it is not in the preview either.
    skills.retain(|skill| !disabled.contains(&skill.name));
    let effective = Config::load().ok().map(|config| config.effective(profile));
    let kind = effective
        .as_ref()
        .and_then(|effective| effective.sandbox_kind());
    let model = effective
        .as_ref()
        .map(|effective| effective.model.clone())
        .unwrap_or_default();
    // The preview has no assembly to name the sandbox-side path, so
    // the host path stands in — the mount postures share it, and the
    // http kind differs only in its constant prefix.
    let (environment, user) = prompt_sections(
        &workspace,
        &workspace.display().to_string(),
        kind.as_deref(),
        model.as_deref().unwrap_or(""),
        HARNESS,
    );
    Ok(rhema_coding_agent::stamped_system_with_sections(
        resolve_base_system(path).as_deref(),
        &workspace,
        &extra,
        &skills,
        environment.as_deref(),
        user.as_deref(),
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
fn failed_checkpoint_path(workspace: &std::path::Path) -> std::path::PathBuf {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_secs())
        .unwrap_or_default();
    workspace
        .join(".rhema")
        .join("runs")
        .join(format!("failed-{secs}.json"))
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

#[cfg(test)]
mod exit_tests {
    use super::*;
    use rhema_coding_agent::CodingAgentError;
    use rhema_core::ProviderError;

    #[test]
    fn a_provider_failure_is_exit_code_2() {
        let err = anyhow::Error::new(CodingAgentError::Agent(rhema_core::AgentError::Provider(
            ProviderError::Api {
                message: "anthropic api error (HTTP 429): rate_limit_error".into(),
                status: Some(429),
            },
        )));
        assert_eq!(exit_code(&err), 2);
    }

    #[test]
    fn a_wrapped_provider_failure_still_reads_as_2() {
        // The chain, not the top error, decides: the failure surfaces
        // through whatever context wrapped it on the way up.
        let err = anyhow::Error::new(CodingAgentError::Agent(rhema_core::AgentError::Provider(
            ProviderError::Timeout,
        )))
        .context("the turn failed");
        assert_eq!(exit_code(&err), 2);
    }

    #[test]
    fn any_other_failure_is_exit_code_1() {
        assert_eq!(exit_code(&anyhow::anyhow!("no prompt")), 1);
        let maxed = anyhow::Error::new(CodingAgentError::Agent(
            rhema_core::AgentError::MaxIterations(8),
        ));
        assert_eq!(exit_code(&maxed), 1, "a step budget is a rhema limit");
    }

    #[test]
    fn the_failure_checkpoint_lives_under_the_workspace_rhema() {
        let path = failed_checkpoint_path(std::path::Path::new("/tmp/ws"));
        let text = path.to_string_lossy();
        assert!(text.starts_with("/tmp/ws/.rhema/runs/failed-"), "{text}");
        assert!(text.ends_with(".json"), "{text}");
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

        let prompt = system_show_at(Some(&base), workspace.path(), None).unwrap();

        let canonical = std::fs::canonicalize(workspace.path()).unwrap();
        assert!(
            prompt.starts_with(&format!("ws {}", canonical.display())),
            "{prompt}"
        );
        assert!(prompt.contains("skill local"), "{prompt}");
        assert!(prompt.contains("tool bash"), "{prompt}");
        // The section renders only where the template places it: this
        // test's own base never references it, so it stays empty — the
        // built-in template's preview below carries it instead.
        assert!(!prompt.contains("## Environment"), "{prompt}");
    }

    #[test]
    fn show_falls_back_to_the_built_in_default_without_a_file() {
        let workspace = tempfile::tempdir().unwrap();

        let prompt = system_show_at(None, workspace.path(), None).unwrap();

        assert!(prompt.contains("bash"), "{prompt}");
        assert!(!prompt.contains("{{"), "the default is rendered: {prompt}");
        assert!(prompt.contains("## Environment"), "{prompt}");
    }

    #[test]
    fn markdown_ansi_styles_and_resets_per_span() {
        let theme = crate::palette::Palette::Rhema.theme(
            crate::palette::ThemeKind::Dark,
            crate::palette::PaletteInputs {
                m3_source: None,
                custom: None,
            },
        );
        let styled = markdown_ansi("# Title\n\nplain text", &theme, 80);
        // Style codes open, the reset closes, and no style bleeds past
        // its span into the next line.
        assert!(styled.contains('\u{1b}'), "styled at all: {styled:?}");
        assert!(styled.contains("\u{1b}[0m"), "a reset per span: {styled:?}");
        assert!(styled.ends_with('\n'));
        // Plain text alone carries no styling of its own.
        let plain = markdown_ansi("just words", &theme, 80);
        assert!(!plain.contains('\u{1b}'), "{plain:?}");
    }

    #[test]
    fn the_sections_gate_routes_kinds_to_host_or_notice() {
        // The local case probes the host — either honest answer; the
        // user section rides the machine's own USER.md, so its presence
        // is not asserted.
        let (host, _) = prompt_sections(std::env::temp_dir().as_path(), "/ws", None, "m", "h");
        let host = host.unwrap();
        assert!(host.contains("## Environment"));
        assert!(host.contains("- Model: m"));
        let (notice, _) = prompt_sections(
            std::env::temp_dir().as_path(),
            "/ws",
            Some("docker"),
            "m",
            "h",
        );
        let notice = notice.unwrap();
        assert!(notice.contains("docker sandbox"));
        assert!(notice.contains("- Workspace: /ws"));
    }
}
