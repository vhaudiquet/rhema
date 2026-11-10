//! The shared chrome of rhema's full-screen menus: the panel they sit
//! in, the row markers they select with, the rule under a heading.
//! Settings, the pickers, and the onboarding wizard all draw through
//! here, so a border style or marker glyph changes everywhere at once —
//! the coherence is in the calls, not in five copies of a block
//! builder.

use crate::palette::Theme;
use ratatui::style::{Style, Stylize};
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
