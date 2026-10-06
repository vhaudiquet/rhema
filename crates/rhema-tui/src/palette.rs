//! The Jadeite design tokens: one place for every color the TUI uses.
//!
//! Source: the Rhema design spec ("Jadeite"). The dark ground is
//! deliberately jade-tinted — that is a decision, not an oversight; do not
//! normalize it to slate. Dark is the primary theme; light exists for
//! light-terminal users and is selected with `--theme light`. Components
//! reference tokens, never literal hex (rule 1); saturated color stays on
//! the accent and the status set (rules 3-4).

use ratatui::style::Color;

/// One theme's tokens, resolved to terminal colors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Theme {
    /// Page canvas.
    pub ground: Color,
    /// Panels and cards.
    pub surface: Color,
    /// Popovers, hovered rows.
    pub elevated: Color,
    /// Code blocks, log view, the transcript well.
    pub sunken: Color,
    /// Hairlines.
    pub border: Color,
    /// Dividers that must read.
    pub border_strong: Color,
    /// Body text.
    pub text: Color,
    /// Secondary text.
    pub text2: Color,
    /// Hints only — floors out here.
    pub text3: Color,
    /// Fills, rules, the mark.
    pub accent: Color,
    /// Accent as text or icon.
    pub accent_text: Color,
    /// Labels ON an accent fill.
    pub on_accent: Color,
    /// A call in flight.
    pub running: Color,
    /// Success.
    pub ok: Color,
    /// Caution.
    pub warn: Color,
    /// A real failure.
    pub error: Color,
    /// Queued, idle, neutral notice.
    pub info: Color,
}

/// The dark theme: the default.
pub const DARK: Theme = Theme {
    ground: Color::Rgb(0x0F, 0x14, 0x13),
    surface: Color::Rgb(0x17, 0x1D, 0x1B),
    elevated: Color::Rgb(0x1F, 0x26, 0x24),
    sunken: Color::Rgb(0x0A, 0x0F, 0x0E),
    border: Color::Rgb(0x28, 0x31, 0x2E),
    border_strong: Color::Rgb(0x3D, 0x48, 0x44),
    text: Color::Rgb(0xEE, 0xF3, 0xF1),
    text2: Color::Rgb(0xB9, 0xC6, 0xC1),
    text3: Color::Rgb(0x87, 0x94, 0x8F),
    accent: Color::Rgb(0x35, 0xA8, 0x82),
    accent_text: Color::Rgb(0x64, 0xC9, 0xA3),
    on_accent: Color::Rgb(0x07, 0x11, 0x0D),
    running: Color::Rgb(0x64, 0xC9, 0xA3),
    ok: Color::Rgb(0x4F, 0xBF, 0x94),
    warn: Color::Rgb(0xD9, 0xA6, 0x43),
    error: Color::Rgb(0xE4, 0x68, 0x7A),
    info: Color::Rgb(0x8F, 0xA6, 0xB8),
};

/// The light theme, for light-terminal users.
pub const LIGHT: Theme = Theme {
    ground: Color::Rgb(0xF8, 0xFA, 0xF9),
    surface: Color::Rgb(0xFF, 0xFF, 0xFF),
    elevated: Color::Rgb(0xFF, 0xFF, 0xFF),
    sunken: Color::Rgb(0xEE, 0xF3, 0xF1),
    border: Color::Rgb(0xDC, 0xE5, 0xE2),
    border_strong: Color::Rgb(0xC3, 0xD2, 0xCD),
    text: Color::Rgb(0x10, 0x18, 0x15),
    text2: Color::Rgb(0x43, 0x53, 0x4E),
    text3: Color::Rgb(0x5F, 0x6D, 0x68),
    accent: Color::Rgb(0x14, 0x6B, 0x4E),
    accent_text: Color::Rgb(0x14, 0x6B, 0x4E),
    on_accent: Color::Rgb(0xFF, 0xFF, 0xFF),
    running: Color::Rgb(0x14, 0x6B, 0x4E),
    ok: Color::Rgb(0x14, 0x6B, 0x4E),
    warn: Color::Rgb(0x8A, 0x5D, 0x12),
    error: Color::Rgb(0xB3, 0x35, 0x4A),
    info: Color::Rgb(0x4A, 0x60, 0x70),
};
