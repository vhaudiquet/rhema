//! The command palette overlay: the filtered rows drawn above the input.

use super::*;

/// The completion popup: while the input names commands or the cursor
/// sits on a path, the matches float above it — full terminal width, the
/// shared background, no panel. The command reads in accent, the
/// description dim, and the highlighted row carries the elevated tone.
/// Tab completes, enter runs.
pub(crate) fn render_command_palette(
    frame: &mut Frame,
    app: &mut App,
    input_area: Rect,
    theme: &Theme,
) {
    let profiles = app.palette_profiles();
    let sandboxes = app.palette_sandboxes();
    let rows = crate::screens::palette_rows_cached(
        &mut app.chat,
        &app.models,
        &profiles,
        &sandboxes,
        app.icons,
    );
    if rows.is_empty() {
        return;
    }
    let visible = rows.len().min(6) as u16;
    let area = Rect {
        x: input_area.x,
        y: input_area.y.saturating_sub(visible + 2),
        width: input_area.width,
        height: visible + 2,
    };
    // Clear erases the transcript beneath; the well tone is painted back
    // so the rows sit on the shared background, framed by a hairline.
    frame.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(ratatui::widgets::BorderType::Rounded)
        .border_style(Style::default().fg(theme.border))
        .style(Style::default().bg(theme.sunken));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    // The window follows the selection: more rows than lines scroll the
    // list once the highlight passes the bottom, instead of hiding it.
    let count = rows.len();
    let visible = visible as usize;
    let first = if count > visible {
        (app.chat.palette_index + 1)
            .saturating_sub(visible)
            .clamp(0, count - visible)
    } else {
        0
    };
    let selected = app.chat.palette_index.min(count - 1);
    // Descriptions align in a column: every name is padded to the longest
    // visible name plus a three-space gap.
    let shown = &rows[first..first + visible];
    let name_width = shown
        .iter()
        .map(|row| palette_row_name(row).width())
        .max()
        .unwrap_or(0)
        + 3;
    let lines: Vec<Line> = shown
        .iter()
        .enumerate()
        .map(|(index, row)| {
            // The selection lives in the text, not the row: bold marks it,
            // the background stays shared.
            let selected_row = first + index == selected;
            let (name, description) = palette_row_texts(row);
            let mut spans = vec![Span::styled(
                format!("{:<name_width$}", name),
                if selected_row {
                    Style::default().fg(theme.accent_text).bold()
                } else {
                    Style::default().fg(theme.accent_text)
                },
            )];
            spans.push(Span::styled(description, Style::default().fg(theme.text3)));
            Line::from(spans)
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), inner);
}

/// The palette row's name column: the command's name, the model's
/// provider-prefixed id — the same shape the picker's rows carry — the
/// profile's name, the sandbox word, or the path entry's name.
pub(crate) fn palette_row_name(row: &crate::screens::PaletteRow) -> String {
    match row {
        crate::screens::PaletteRow::Command(spec) => spec.name.to_owned(),
        crate::screens::PaletteRow::Model { provider, id, .. } => format!("{provider}/{id}"),
        crate::screens::PaletteRow::Profile { name, .. } => name.clone(),
        crate::screens::PaletteRow::Choice { value, .. } => (*value).to_owned(),
        crate::screens::PaletteRow::Sandbox { value, .. } => value.clone(),
        crate::screens::PaletteRow::Path { name, .. } => name.clone(),
    }
}

/// The palette row's description: the command's own, the model's display
/// name with its facts, the profile's provider and model, the sandbox's
/// kind and target, or the path entry's kind.
pub(crate) fn palette_row_texts(row: &crate::screens::PaletteRow) -> (String, String) {
    match row {
        crate::screens::PaletteRow::Command(spec) => {
            (palette_row_name(row), spec.description.to_string())
        }
        crate::screens::PaletteRow::Model { description, .. } => {
            (palette_row_name(row), description.clone())
        }
        crate::screens::PaletteRow::Profile { description, .. } => {
            (palette_row_name(row), description.clone())
        }
        crate::screens::PaletteRow::Choice { description, .. } => {
            (palette_row_name(row), (*description).to_owned())
        }
        crate::screens::PaletteRow::Sandbox { description, .. } => {
            (palette_row_name(row), description.clone())
        }
        crate::screens::PaletteRow::Path { is_dir, .. } => (
            palette_row_name(row),
            if *is_dir {
                "directory".to_owned()
            } else {
                String::new()
            },
        ),
    }
}
