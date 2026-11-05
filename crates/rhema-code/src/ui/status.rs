//! The status strip above the input: the spinner or error line, the live
//! subagent rows, the thinking preview, and the cost.

use super::*;

/// The dim telemetry strip under the input: last turn's tokens, and the
/// history-pinned indicator.
pub(crate) fn render_status(frame: &mut Frame, app: &App, area: Rect, theme: &Theme) {
    let width = area.width as usize;
    let mut lines: Vec<Line> = if let Some(error) = &app.error {
        // Wrapped a cell short so the indent below cannot push a wrap.
        wrap_line(&Line::from(error.clone()), width.saturating_sub(1))
            .into_iter()
            .map(|line| line.style(Style::default().fg(theme.error).bold()))
            .collect()
    } else if app.turn.running {
        // Braille dots cycling through the classic spinner, animated off
        // the running turn's clock — no timer machinery of its own.
        let spinner = spinner_frame(
            app.turn
                .started_at
                .map(|started| started.elapsed())
                .unwrap_or_default(),
        );
        // The phase slot, three states: a retry the ladder is counting
        // down (the error in the strip's alarm color, the wait local —
        // one event announced it, the strip ticks the clock), a command
        // executing, the model streaming.
        let phase = if let Some(wait) = &app.turn.retry_wait {
            let remaining = wait
                .deadline
                .saturating_duration_since(std::time::Instant::now());
            Line::from(vec![
                Span::styled(
                    SPINNER[spinner].to_string(),
                    Style::default().fg(theme.error),
                ),
                Span::styled(format!(" {}", wait.error), Style::default().fg(theme.error)),
                Span::raw(format!(
                    " · attempt {} of {} failed · retrying in {}",
                    wait.attempt,
                    wait.total,
                    elapsed_text(remaining)
                ))
                .fg(theme.text3),
            ])
        } else if app.turn.processing {
            Line::from(vec![
                Span::styled(
                    SPINNER[spinner].to_string(),
                    Style::default().fg(theme.info),
                ),
                Span::raw(" running").fg(theme.info),
            ])
        } else {
            Line::from(vec![
                Span::styled(
                    SPINNER[spinner].to_string(),
                    Style::default().fg(theme.running),
                ),
                Span::raw(" thinking").fg(theme.running),
            ])
        };
        let mut line = phase;
        if let Some(started) = app.turn.started_at {
            line.spans
                .push(Span::raw(format!(" · {}", elapsed_text(started.elapsed()))).fg(theme.text3));
        }
        line.spans.push(Span::raw(" · esc cancels").fg(theme.text3));
        vec![line]
    } else {
        Vec::new()
    };
    // A failed turn that can still be re-run names the way: the dim
    // line rides the error until a submit, a cancel, or a retry clears
    // both.
    if app.retryable {
        lines.push(Line::from("/retry re-sends it").fg(theme.text3));
    }
    // Every live subagent job pins its own line right below the
    // spinner: the robot badge, its phase, spend, and age. The registry
    // is the single source — a job's line lives while it works; when it
    // finishes the line leaves and the transcript carries the outcome
    // as the finish notice.
    for subagent in subagent_lines(app, theme) {
        lines.push(subagent);
    }
    // The live preview rides under the spinner — the reasoning trace's
    // tail, or a running command's earned output — at most three dim
    // rows. The reasoning half is gone the moment real output starts,
    // because the buffer clears then.
    if app.turn.running && app.error.is_none() {
        lines.extend(thinking_preview(app, width, theme));
    }
    // Content sits one cell in, on the transcript's left inset, and a
    // blank row above the strip keeps it off the transcript's tail.
    for line in &mut lines {
        line.spans.insert(0, Span::raw(" "));
    }
    if lines.is_empty() {
        lines.push(Line::from(""));
    } else {
        lines.insert(0, Line::from(""));
    }
    if app.chat.scroll > 0 && app.error.is_none() {
        lines
            .last_mut()
            .unwrap()
            .spans
            .push(Span::raw("  ·  viewing history · pgdn: latest").fg(theme.text3));
    }
    frame.render_widget(Paragraph::new(lines), area);
}

/// The classic braille spinner; frames step at 80ms off whichever clock
/// animates it.
const SPINNER: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

/// The spinner frame for an elapsed time.
pub(crate) fn spinner_frame(elapsed: std::time::Duration) -> usize {
    (elapsed.as_millis() / 80) as usize % SPINNER.len()
}

/// The spinner's current character for an elapsed time — the frame the
/// status strip and the flow dialog both draw.
pub(crate) fn spinner_char(elapsed: std::time::Duration) -> char {
    SPINNER[spinner_frame(elapsed)]
}

/// An elapsed time as a clock reads it: seconds under the minute,
/// minutes and seconds under the hour, hours and minutes beyond —
/// `2055s` is unreadable where `34min 15s` reads at a glance.
pub(crate) fn elapsed_text(elapsed: std::time::Duration) -> String {
    let secs = elapsed.as_secs();
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}min {}s", secs / 60, secs % 60)
    } else {
        format!("{}h {}min", secs / 3600, (secs % 3600) / 60)
    }
}

/// A cost as the footer prints it: cents at a glance, four decimals
/// while the figure is still small enough for the fourth to matter.
pub(crate) fn cost_text(cost: f64) -> String {
    if cost >= 0.01 || cost == 0.0 {
        format!("${cost:.2}")
    } else {
        format!("${cost:.4}")
    }
}

/// The pinned live lines, oldest job first: one per working job —
/// queued or running — on the wire's registry, read fresh every redraw
/// — the strip's layout counts them and its drawing renders them, so
/// the reserve matches what is on screen.
pub(crate) fn subagent_lines(app: &App, theme: &Theme) -> Vec<Line<'static>> {
    app.subagent_jobs()
        .iter()
        .map(|job| subagent_line(app, job, theme))
        .collect()
}

/// The pinned live line of one working subagent job: the robot badge
/// and its spinner, the delegate's name and job id, the phase it last
/// reported, the context its current model call carries against its
/// model's window, the cost its cumulative spend prices to, and its
/// age. A figure the catalog cannot price or size degrades to what is
/// known — tokens alone, no cost.
pub(crate) fn subagent_line(
    app: &App,
    subagent: &rhema_subagents::JobStatus,
    theme: &Theme,
) -> Line<'static> {
    let mut spans = vec![
        Span::raw(format!("{} ", app.icons.robot)).fg(theme.text3),
        Span::styled(
            SPINNER[spinner_frame(subagent.started_at.elapsed())].to_string(),
            Style::default().fg(theme.info),
        ),
        Span::raw(" "),
        Span::raw(format!("{} ({})", subagent.name, subagent.id)).fg(theme.info),
        Span::raw(format!(" · {}", subagent.phase)).fg(theme.running),
    ];
    // The gauge reads the current call only: summed across steps, the
    // re-sent prompts would show a context many times its real size.
    if let Some(context) = subagent.context {
        let mut figure = format_tokens(Some(crate::app::context_tokens(&context)));
        let window = subagent
            .model
            .as_deref()
            .and_then(|model| app.model_window(model));
        if let Some(window) = window {
            figure.push_str(&format!("/{}", format_tokens(Some(window))));
        }
        spans.push(Span::raw(format!(" · {figure}")).fg(theme.text3));
    }
    // The cost reads the cumulative spend: every re-sent prompt bills
    // again, so the spend legitimately outgrows the window.
    if let Some(pricing) = subagent
        .model
        .as_deref()
        .and_then(|model| app.model_pricing(model))
        && let Some(spent) = subagent.spent
    {
        let cost = pricing.cost(
            u64::from(spent.input_tokens),
            u64::from(spent.output_tokens),
            u64::from(spent.cached_input_tokens),
        );
        spans.push(Span::raw(format!(" · {}", cost_text(cost))).fg(theme.text3));
    }
    spans.push(
        Span::raw(format!(
            " · {}",
            elapsed_text(subagent.started_at.elapsed())
        ))
        .fg(theme.text3),
    );
    Line::from(spans)
}

/// Rows the live preview may show.
const THINKING_PREVIEW_ROWS: usize = 3;

/// A running command's output earns its preview after this long.
const TOOL_PREVIEW_DELAY: std::time::Duration = std::time::Duration::from_secs(1);

/// Whether the tool preview shows: a second of runtime or a second
/// completed line — an instant command never flashes its output.
pub(crate) fn tool_preview_ready(app: &App) -> bool {
    let Some(started) = app.turn.tool_output_at else {
        return false;
    };
    started.elapsed() >= TOOL_PREVIEW_DELAY || app.turn.tool_output.matches('\n').count() >= 2
}

/// The live preview's rows at `width`: a running command's output while
/// it has earned the screen, the reasoning trace's tail otherwise. Both
/// the strip's layout and its drawing come from here, so the reserve
/// matches what's on screen.
pub(crate) fn thinking_preview_rows(app: &App, width: usize) -> Vec<Line<'static>> {
    let text = if tool_preview_ready(app) {
        &app.turn.tool_output
    } else {
        &app.turn.thinking
    };
    let width = width.saturating_sub(1).max(1);
    let mut rows: Vec<Line<'static>> = completed_thinking(text)
        .flat_map(|line| wrap_line(&Line::from(Span::raw(line.to_owned())), width))
        .collect();
    let skip = rows.len().saturating_sub(THINKING_PREVIEW_ROWS);
    rows.drain(..skip);
    rows
}

/// The buffer's completed lines: everything it holds before the line
/// still streaming. The preview reveals a line at a time, never a word
/// at a time. Blank lines drop out — the preview has three rows to
/// spend and a blank one carries nothing.
pub(crate) fn completed_thinking(thinking: &str) -> impl Iterator<Item = &str> {
    let complete = match thinking.rfind('\n') {
        Some(end) => &thinking[..end],
        None => "",
    };
    complete.split('\n').filter(|line| !line.is_empty())
}

/// The live preview rows: the tail of the chosen buffer's completed
/// lines, wrapped and dim. Empty until a line completes.
pub(crate) fn thinking_preview(app: &App, width: usize, theme: &Theme) -> Vec<Line<'static>> {
    let style = Style::default().fg(theme.text3).italic();
    thinking_preview_rows(app, width)
        .into_iter()
        .map(|line| line.style(style))
        .collect()
}

/// How many rows the preview occupies at `width` — the same wrap the
/// render applies, so the strip reserves exactly what it draws.
pub(crate) fn thinking_rows(app: &App, width: usize) -> u16 {
    thinking_preview_rows(app, width).len() as u16
}
