//! The shared chrome of rhema's full-screen menus: the panel they sit
//! in, the row markers they select with, the rule under a heading.
//! Settings, the pickers, and the onboarding wizard all draw through
//! here, so a border style or marker glyph changes everywhere at once —
//! the coherence is in the calls, not in five copies of a block
//! builder.

use crate::palette::Theme;
use ratatui::buffer::Buffer;
use ratatui::style::{Color, Style, Stylize};
use ratatui::text::Span;
use ratatui::widgets::{Block, Borders};

/// The panel every full-screen menu sits in: rounded border, the menu's
/// ground, a spaced title. `None` is the untitled frame.
pub fn panel<'a>(title: Option<&str>, theme: &Theme) -> Block<'a> {
    // A native background leaves the panel without its fill, so the
    // frame needs the readable ink to separate itself from the
    // terminal's own ground; on the palette's fill the strong hairline
    // is enough.
    let frame = if theme.is_native() {
        theme.text3
    } else {
        theme.border_strong
    };
    let mut block = Block::default()
        .borders(Borders::ALL)
        .border_type(ratatui::widgets::BorderType::Rounded)
        .border_style(Style::default().fg(frame))
        // Titles draw over the border cells: without ink of their own
        // they inherit the border's fg and read as dim as the frame.
        .title_style(Style::default().fg(theme.text))
        .style(Style::default().bg(theme.sunken).fg(theme.text));
    if let Some(title) = title {
        block = block.title(format!(" {title} "));
    }
    block
}

/// Fades the page under a modal: every cell loses contrast and
/// saturation toward the canvas, so the dialog's own ink is the only
/// crisp color left. Terminals cannot blur glyphs; this haze is the
/// focus cue a cell grid gives.
pub(crate) fn dim_backdrop(buffer: &mut Buffer, theme: &Theme) {
    // A native background leaves the fill slots reset; the ground still
    // names the appearance's canvas, so a fade has a color to aim at.
    let target = if matches!(theme.sunken, Color::Reset) {
        theme.ground
    } else {
        theme.sunken
    };
    let area = buffer.area;
    for y in area.y..area.y.saturating_add(area.height) {
        for x in area.x..area.x.saturating_add(area.width) {
            let Some(cell) = buffer.cell_mut((x, y)) else {
                continue;
            };
            cell.fg = fade(cell.fg, target, 0.55);
            cell.bg = fade(cell.bg, target, 0.30);
        }
    }
}

/// Blends one channel set toward `target`, after softening the color
/// toward its luma. Non-RGB cells (the terminal's own reset ink, indexed
/// colors) stay as they are.
fn fade(color: Color, target: Color, amount: f32) -> Color {
    let (Color::Rgb(r, g, b), Color::Rgb(tr, tg, tb)) = (color, target) else {
        return color;
    };
    let luma = 0.299 * r as f32 + 0.587 * g as f32 + 0.114 * b as f32;
    let mix = |channel: u8, tone: u8| {
        let soft = channel as f32 * 0.72 + luma * 0.28;
        (soft * (1.0 - amount) + tone as f32 * amount).round() as u8
    };
    Color::Rgb(mix(r, tr), mix(g, tg), mix(b, tb))
}

/// The selection marker column every list shares: `❯ ` on the selected
/// row, a blank of the same width elsewhere, in the accent.
pub fn marker(selected: bool, theme: &Theme) -> Span<'static> {
    Span::raw(if selected { "❯ " } else { "  " }).fg(theme.accent_text)
}

/// A horizontal rule the width of its pane, in the strong border tone —
/// the divider under a heading.
pub fn rule(width: usize, theme: &Theme) -> Span<'static> {
    Span::raw("─".repeat(width)).fg(theme.border_strong)
}
