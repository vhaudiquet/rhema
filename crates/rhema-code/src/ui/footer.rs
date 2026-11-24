//! The one-row footer: the model id, the workspace folder, the thinking
//! effort letter, and the live-state hints.

use super::status::cost_text;
use super::*;
use std::path::PathBuf;

/// The model id as the footer shows it: everything after the last `/`,
/// so an OpenRouter id like `apodex/apodex-1.1-mini:free` reads without
/// its vendor prefix. Ids without a slash pass through whole.
pub(crate) fn footer_model(model: &str) -> &str {
    model.rsplit('/').next().unwrap_or(model)
}

/// Parents the footer's folder may show beside the folder itself,
/// three folders in all.
const FOOTER_FOLDER_PARENTS: usize = 2;

/// The workspace as the footer shows it: `(prefix, parents, folder)`.
/// The home prefix is trimmed away, and the home's `Documents` with
/// it, where projects live, the word carries nothing, then at most
/// [`FOOTER_FOLDER_PARENTS`] parents ride beside the folder itself so a
/// deep path stays small. `prefix` is `/` for a whole absolute path,
/// `…/` when parents fell off. A path with nothing left after the trim
/// (the home itself, its `Documents`) falls back to its own name.
pub(crate) fn footer_folder(
    workspace: &Path,
    home: Option<&Path>,
) -> (String, Vec<String>, String) {
    // The windows spelling rides as the unix one: the separators
    // normalize first, so the home trim and the parent cap read the
    // same rule on every host, and the footer always spells '/'.
    let mut rel = PathBuf::from(workspace.to_string_lossy().replace('\\', "/"));
    if let Some(home) = home {
        let home = PathBuf::from(home.to_string_lossy().replace('\\', "/"));
        if let Ok(stripped) = rel.strip_prefix(&home) {
            rel = stripped.to_path_buf();
        }
        if let Ok(stripped) = rel.strip_prefix("Documents") {
            rel = stripped.to_path_buf();
        }
    }
    let text = rel.to_string_lossy();
    // Absolute by either spelling: a leading separator, or a drive
    // letter, the windows shape a unix host still renders.
    let absolute = text.starts_with('/') || text.chars().nth(1) == Some(':');
    let names: Vec<&str> = text
        .split('/')
        .filter(|name| {
            // The drive component (`D:`) is spelling, not a folder.
            !name.is_empty() && !(name.len() == 2 && name.ends_with(':'))
        })
        .collect();
    let Some(folder) = names.last() else {
        let fallback = workspace
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "/".to_owned());
        return (String::new(), Vec::new(), fallback);
    };
    let truncated = names.len() > FOOTER_FOLDER_PARENTS + 1;
    let keep = names.len().min(FOOTER_FOLDER_PARENTS + 1);
    let parents: Vec<String> = names[names.len() - keep..names.len() - 1]
        .iter()
        .map(|name| (*name).to_owned())
        .collect();
    let prefix = if truncated {
        "…/".to_owned()
    } else if absolute {
        "/".to_owned()
    } else {
        String::new()
    };
    (prefix, parents, (*folder).to_owned())
}

/// The terminal window title: the mark, then the workspace folder as
/// the footer spells it, the home trim and the capped parents, so
/// the tab reads like the bar's own start. The folder badge glyph
/// stays out: title bars draw plain text, and a badge that renders on
/// the screen arrives as tofu in a macOS tab. Color and the folder's
/// bold are lost with it; the bar's leading margin space does not
/// ride along either.
///
/// A working turn leads with the status strip's spinner frame, so a
/// tab running rhema reads as busy from the tab bar. A turn parked on
/// an approval does not spin: it waits on the user, not the model.
pub(crate) fn window_title(app: &App) -> String {
    let (prefix, parents, folder) = footer_folder(&app.options.workspace, app.home.as_deref());
    let mut title = String::new();
    if app.turn.running && app.turn.pending_approval.is_none() {
        let elapsed = app
            .turn
            .started_at
            .map(|started| started.elapsed())
            .unwrap_or_default();
        title.push(super::status::spinner_char(elapsed));
        title.push(' ');
    }
    title.push_str("ῥ · ");
    title.push_str(&prefix);
    for parent in &parents {
        title.push_str(parent);
        title.push('/');
    }
    title.push_str(&folder);
    title
}

/// The thinking badge's letter: the glyph reads with one letter beside
/// it, L for low through H for high.
pub(crate) fn effort_letter(effort: rhema_core::request::ThinkingEffort) -> &'static str {
    use rhema_core::request::ThinkingEffort;
    match effort {
        ThinkingEffort::Default => "",
        ThinkingEffort::Low => "L",
        ThinkingEffort::Medium => "M",
        ThinkingEffort::High => "H",
    }
}

pub(crate) fn render_footer(frame: &mut Frame, app: &App, area: Rect, theme: &Theme) {
    // The mark, then always the same facts: profile, where the session
    // sits, model, where bash runs, context footprint against the
    // model's window, the session's accumulated cost.
    // Every segment owns its leading separator, a trailing one on the
    // previous segment doubles as soon as an optional segment (folder,
    // allowance) sits between two others.
    let effective = app.effective();
    let mut parts = vec![Span::raw(" ῥ").fg(theme.accent_text)];
    if let Some((name, color)) = app.active_profile() {
        parts.push(Span::raw(" · ").fg(theme.text3));
        parts.push(Span::raw(name).fg(color));
    }
    // Where the session sits: the folder badge over the trimmed
    // workspace path, its folder in bold.
    let (prefix, parents, folder) = footer_folder(&app.options.workspace, app.home.as_deref());
    parts.push(Span::raw(format!(" · {} ", app.icons.folder)).fg(theme.text3));
    parts.push(Span::raw(prefix).fg(theme.text3));
    for parent in parents {
        parts.push(Span::raw(parent).fg(theme.text3));
        parts.push(Span::raw("/").fg(theme.text3));
    }
    parts.push(Span::raw(folder).fg(theme.text2).bold());
    match (&effective.provider, &effective.model) {
        // The missing pieces read as failures, not hints: red, the way
        // a broken build reads, so the fix is the first thing the eye
        // finds.
        (None, _) => parts.push(Span::raw(" · no provider; use /login").fg(theme.error)),
        (_, None) => parts.push(Span::raw(" · no model; use /model").fg(theme.error)),
        // The id's vendor prefix is picker detail: a footer reads the
        // bare model name.
        (_, Some(model)) => {
            parts.push(Span::raw(" · ").fg(theme.text3));
            parts.push(Span::raw(footer_model(model).to_owned()).fg(theme.text2));
        }
    }
    // Where bash runs: the sandbox kind, or the running instance's
    // name when the profile's active pointer names it. Local reads as
    // a fact too, the fast path is a choice, not an absence.
    parts.push(
        Span::raw(format!(" · {} {}", app.icons.sandbox, app.sandbox_badge())).fg(theme.text3),
    );
    // A named thinking effort is session-relevant telemetry: the badge
    // only appears when the level would change the request. The letter
    // rides the glyph; the word it abbreviates is the /thinking
    // argument, one keystroke away in the palette.
    if app.options.thinking != rhema_core::request::ThinkingEffort::Default {
        parts.push(
            Span::raw(format!(
                " · {} {}",
                app.icons.thinking,
                effort_letter(app.options.thinking)
            ))
            .fg(theme.text3),
        );
    }
    let used = app.context_used();
    let limit = app.current_context_window();
    // The gauge is the telemetry: the glyph itself fills with the
    // window, readable before the numbers are.
    let fraction = limit
        .map(|limit| used as f64 / limit as f64)
        .unwrap_or(0.0)
        .clamp(0.0, 1.0);
    let mut context = format_tokens(Some(used));
    if let Some(limit) = limit {
        context.push_str(&format!("/{}", format_tokens(Some(limit))));
        // The fill as a number beside the glyph: the glyph shows the
        // bucket, the percentage the exact read.
        context.push_str(&format!(" ({:.1}%)", fraction * 100.0));
    }
    parts
        .push(Span::raw(format!(" · {} {context}", (app.icons.context)(fraction))).fg(theme.text3));
    parts.push(Span::raw(format!(" · {}", cost_text(app.session_cost))).fg(theme.text3));
    // The plan's remaining allowance, when the active backend reports
    // one: the adapter owns the figures, the footer only prints them.
    // A kind names the unit where the amount alone would read as
    // noise ("238/300"); a money amount needs no word beside it.
    let active = app.backend.as_ref().map(crate::app::Backend::config_name);
    if let Some((provider, Some(allowance))) = &app.subscription
        && Some(*provider) == active
    {
        let mut badge = format!("{} {}", app.icons.allowance, allowance.amount);
        if !allowance.kind.is_empty() {
            badge.push(' ');
            badge.push_str(&allowance.kind);
        }
        parts.push(Span::raw(format!(" · {badge}")).fg(theme.text3));
    }
    if app.chat.scroll > 0 && app.error.is_none() {
        parts.push(Span::raw(" · viewing history").fg(theme.text3));
    }
    frame.render_widget(Paragraph::new(Line::from(parts).left_aligned()), area);
}
