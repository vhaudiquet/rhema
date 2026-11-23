//! The `rhema system` utilities and their renderer: the prompt
//! preview's markdown-to-ANSI path, the theme it picks from config,
//! and the edit/restore/show actions, all model-free and provider-free.
//! `show` reads the keychain once, and only when the profile names a
//! search provider: the section must say what the session would wire,
//! and a key that lives only there still wires.

use super::*;

/// The `rhema system` utilities: they shape what turns would send and
/// run no model, no provider, profile, or credential is touched.
pub(crate) fn system_command(
    action: SystemCommand,
    workspace: &std::path::Path,
    profile: Option<&str>,
) -> anyhow::Result<()> {
    match action {
        SystemCommand::Edit => {
            let path = rhema_config::Home::discover()?.system_prompt();
            system_edit_at(&path, None)
        }
        SystemCommand::Restore => {
            let path = rhema_config::Home::discover()?.system_prompt();
            system_restore_at(&path)
        }
        SystemCommand::Show => {
            // A preview that silently lacks a section reads as a bug in
            // the prompt. When the backend is installed but too old to
            // serve `job`, say so on stderr and keep stdout byte-exact.
            if let Some(problem) = rhema_bash::jobs::backend_problem() {
                eprintln!("warning: background jobs disabled: {problem}");
            }
            // The session's own keychain decides whether the search
            // section exists: a key that lives only there still wires
            // in the session, so the preview reads it. The read only
            // happens when a profile names search instances.
            let secrets = rhema_config::secrets::KeyringSecretStore::named(
                rhema_config::secrets::RHEMA_SERVICE,
            );
            let prompt = system_show_at(
                rhema_config::Home::discover()
                    .ok()
                    .map(|home| home.system_prompt())
                    .as_deref(),
                workspace,
                profile,
                Some(&secrets),
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

/// The stored look's theme, the same resolution the TUI makes, for the
/// one off-TUI surface that prints styled markdown.
pub(crate) fn theme_from_config(config: &Config) -> crate::palette::Theme {
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

/// Renders markdown to styled ANSI text through the TUI's own renderer,
/// so `system show` on a terminal reads like the transcript it previews.
/// One reset per span keeps a stray style from bleeding past its line.
pub(crate) fn markdown_ansi(source: &str, theme: &crate::palette::Theme, width: usize) -> String {
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
            out.push_str(&without_terminal_controls(span.content.as_ref()));
            if style.fg.is_some() || !style.add_modifier.is_empty() {
                out.push_str(&crossterm::style::ResetColor.to_string());
            }
        }
        out.push('\n');
    }
    out
}

/// Drops control characters from prompt text before it reaches the
/// terminal. The prompt can be workspace-owned, and a raw ESC there
/// would drive the terminal (title changes, clipboard writes,
/// hyperlinks). Newline and tab stay: they shape text, they do not
/// execute.
pub(crate) fn without_terminal_controls(text: &str) -> String {
    text.chars()
        .filter(|c| !c.is_control() || matches!(c, '\n' | '\t'))
        .collect()
}

/// Maps a ratatui color onto the crossterm one of the same terminal
/// slot, the two crates disagree on which half of each pair is the
/// plain name, so the mapping is spelled out.
pub(crate) fn crossterm_color(color: ratatui::style::Color) -> crossterm::style::Color {
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
/// default when missing, then opens the editor on it. `editor`, the
/// tests' seam, overrides `$EDITOR`/`$VISUAL`.
pub(crate) fn system_edit_at(path: &std::path::Path, editor: Option<&str>) -> anyhow::Result<()> {
    if !path.exists() {
        seed_system_file(path)?;
        eprintln!("· seeded {} with the built-in default", path.display());
    }
    run_editor(path, editor)
}

/// `rhema system restore`: the config's SYSTEM.md becomes the built-in
/// default again.
pub(crate) fn system_restore_at(path: &std::path::Path) -> anyhow::Result<()> {
    seed_system_file(path)?;
    eprintln!("· restored {} to the built-in default", path.display());
    Ok(())
}

/// The prompt the next turn in `workspace` would send: the config's
/// SYSTEM.md when present, the built-in default otherwise, through the
/// same assembly the agent stamps, rendered over the workspace, its
/// skills, the tools, and the two optional sections. The sandbox kind
/// resolves from the stored profile the way startup does, so the
/// preview and the first turn agree on host facts versus the notice.
pub(crate) fn system_show_at(
    path: Option<&std::path::Path>,
    workspace: &std::path::Path,
    profile: Option<&str>,
    secrets: Option<&dyn rhema_config::secrets::SecretStore>,
) -> anyhow::Result<String> {
    let mcp = mcp_preview_lines(profile);
    system_show_at_with_mcp(path, workspace, profile, mcp, secrets)
}

/// The preview over explicit mcp lines, the test seam. Production
/// derives them from config ([`mcp_preview_lines`]); a test injects
/// its own rather than reading the machine's real `mcp.yaml`.
pub(crate) fn system_show_at_with_mcp(
    path: Option<&std::path::Path>,
    workspace: &std::path::Path,
    profile: Option<&str>,
    mcp: Vec<rhema_config::mcp::McpServerLine>,
    secrets: Option<&dyn rhema_config::secrets::SecretStore>,
) -> anyhow::Result<String> {
    let workspace = std::fs::canonicalize(workspace).unwrap_or_else(|_| workspace.to_path_buf());
    let home = rhema_config::Home::discover().ok();
    // The preview shows what turns send: a disabled subagent is not in
    // the catalog, so it is not in the preview either.
    let disabled_subagents = Config::load()
        .ok()
        .map(|c| c.disabled_subagents.clone())
        .unwrap_or_default();
    // No provider factory here: the system utilities touch no
    // credentials, so the preview's spec names no cross-provider
    // reach even when the session would list some.
    let mut extra = rhema_subagents::extra_specs(&workspace, &disabled_subagents, &[]);
    // The transcript is on by default, so the default preview names
    // zoom, exactly as the agent's prompt does.
    extra.push(rhema_transcript::Zoom::spec());
    let agents = home.as_ref().map(|home| home.agents_skills());
    let global = home.as_ref().map(|home| home.skills());
    let disabled = Config::load()
        .ok()
        .map(|c| c.settings.skills_disabled.clone())
        .unwrap_or_default();
    let mut skills = rhema_skills::discover(agents.as_deref(), global.as_deref(), &workspace);
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
    // the host path stands in, the mount postures share it, and the
    // http kind differs only in its constant prefix.
    let (environment, user, _) = prompt_sections(
        &workspace,
        &workspace.display().to_string(),
        kind.as_deref(),
        model.as_deref().unwrap_or(""),
        HARNESS,
    );
    // The mcp lines come from config alone, the same fold the wire
    // step runs, minus the runtime: no bridge, no spawns. A preview
    // must never have side effects, so the lines are what the
    // session WOULD disclose, not what it has.
    let mut sections: Vec<(&str, serde_json::Value)> = Vec::new();
    if let Some(text) = environment.as_deref() {
        sections.push(("environment", serde_json::json!(text)));
    }
    if let Some(text) = user.as_deref() {
        sections.push(("user", serde_json::json!(text)));
    }
    if !mcp.is_empty() {
        sections.push(("mcp", serde_json::json!(mcp)));
    }
    // The three local sections, gated the way the wire gates them: a
    // sandboxed session runs its commands on the far side, where none
    // of the three functions was installed, and setup skips each
    // provisioning step before it renders anything. A preview that
    // showed them would promise a `see`, a `job`, or a `search` the
    // session does not have.
    if kind.is_none() {
        // Vision is static text a local session always carries.
        sections.push((
            "vision",
            serde_json::json!(rhema_bash::vision::PROMPT_SECTION),
        ));
        // Jobs may be missing outright: its backend is a separate
        // install, so the preview asks the same probe the wire uses.
        if rhema_bash::jobs::available() {
            sections.push(("jobs", serde_json::json!(rhema_bash::jobs::PROMPT_SECTION)));
        }
        // Search exists only when a provider and its key resolve:
        // the wire's own gate, without building a provider. The
        // keychain is read here, the one credential touch the
        // utilities make, and only when the profile names search
        // instances at all.
        if search_section(kind.as_deref(), effective.as_ref(), secrets) {
            sections.push((
                "websearch",
                serde_json::json!(rhema_bash::websearch::PROMPT_SECTION),
            ));
        }
        // Decision rides the same local rule the search section does:
        // the section exists exactly when the session would wire the
        // `decide` macro, the wire's own gate, asked without
        // provisioning the init file.
        if decision_section(kind.as_deref(), effective.as_ref(), secrets) {
            sections.push((
                "decision",
                serde_json::json!(rhema_bash::decision::PROMPT_SECTION),
            ));
        }
    }
    Ok(rhema_host_agent::stamped_system_with_sections(
        resolve_base_system(path).as_deref(),
        &workspace,
        &extra,
        &skills,
        &sections,
    )?)
}

/// The search section's rule: a local session, and the wire's own
/// gate open. Setup asks the gate once, in `wire_for_session`; the
/// preview asks the same question here, with the session's keychain
/// rather than its own provisioning.
pub(crate) fn search_section(
    kind: Option<&str>,
    profile: Option<&rhema_config::Profile>,
    secrets: Option<&dyn rhema_config::secrets::SecretStore>,
) -> bool {
    kind.is_none() && rhema_websearch_registry::would_wire(profile, secrets)
}

/// The decision section's rule: a local session, and the decision
/// wire's own gate open, the same two gates
/// [`crate::decision_wire::wire_for_session`] closes on. The preview
/// asks the question here rather than wiring, so no init file is
/// written for a prompt nobody sends.
pub(crate) fn decision_section(
    kind: Option<&str>,
    profile: Option<&rhema_config::Profile>,
    secrets: Option<&dyn rhema_config::secrets::SecretStore>,
) -> bool {
    kind.is_none() && rhema_decision_registry::is_configured(profile, secrets)
}

/// The prompt's mcp lines as config would produce them: global file
/// plus the profile's own entries, profile winning by name, the
/// muted set filtered out, invalid entries skipped as the wire step
/// skips them. No bridge, no spawns, a preview, not a wiring.
fn mcp_preview_lines(profile: Option<&str>) -> Vec<rhema_config::mcp::McpServerLine> {
    let config = match Config::load() {
        Ok(config) => config,
        Err(_) => return Vec::new(),
    };
    if config.disabled_mcp_servers.contains(&String::from("*")) {
        return Vec::new();
    }
    let effective = config.effective(profile);
    let merged = crate::mcp_wire::merged_config(&effective);
    merged
        .servers
        .iter()
        .filter(|server| !config.disabled_mcp_servers.contains(&server.name))
        .map(rhema_config::mcp::McpServer::prompt_line)
        .collect()
}

/// Writes the built-in default to `path`, creating the directory. The
/// default goes out verbatim: the file is a template, injections
/// render per turn, never here.
pub(crate) fn seed_system_file(path: &std::path::Path) -> anyhow::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, rhema_host_agent::DEFAULT_SYSTEM_MD)?;
    Ok(())
}

/// Runs the editor on `path`, blocking until it closes. The editor
/// value may carry flags (`code -w`), so it goes through `sh -c` with
/// the file as the only positional argument; a non-zero exit fails.
pub(crate) fn run_editor(path: &std::path::Path, editor: Option<&str>) -> anyhow::Result<()> {
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
