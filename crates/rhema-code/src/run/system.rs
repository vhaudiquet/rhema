//! The `rhema system` utilities and their renderer: the prompt
//! preview's markdown-to-ANSI path, the theme it picks from config,
//! and the edit/restore/show actions — all model-free, no provider
//! or credential touched.

use super::*;

/// The `rhema system` utilities: they shape what turns would send and
/// run no model — no provider, profile, or credential is touched.
pub(crate) fn system_command(
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
/// default when missing, then opens the editor on it. `editor` — the
/// tests' seam — overrides `$EDITOR`/`$VISUAL`.
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
/// same assembly the agent stamps — rendered over the workspace, its
/// skills, the tools, and the two optional sections. The sandbox kind
/// resolves from the stored profile the way startup does, so the
/// preview and the first turn agree on host facts versus the notice.
pub(crate) fn system_show_at(
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
        &[],
    )?)
}

/// Writes the built-in default to `path`, creating the directory. The
/// default goes out verbatim: the file is a template, injections
/// render per turn, never here.
pub(crate) fn seed_system_file(path: &std::path::Path) -> anyhow::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, rhema_coding_agent::DEFAULT_SYSTEM_MD)?;
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
