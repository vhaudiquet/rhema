//! Color palettes: one place for every color the TUI uses.
//!
//! A palette is a named set of [`Theme`] tokens in a dark and a light
//! appearance. The default is the Rhema palette, the design spec
//! ("Jadeite") the TUI was drawn against; the others restyle the same
//! token slots. The dark grounds are deliberately tinted — that is a
//! decision, not an oversight; do not normalize them to slate. Dark is
//! the primary appearance; light exists for light-terminal users and is
//! selected with `theme: light`. Components reference tokens, never
//! literal hex (rule 1); saturated color stays on the accent and the
//! status set (rules 3-4). M3 You is the one generated palette: its
//! tokens come from the tonal-spot scheme in [`crate::m3`], not from
//! constants here.
//!
//! The Catppuccin palettes take their hexes from the official palette
//! and their roles from the Catppuccin style guide (base for the pane,
//! mantle and crust for secondary panes, surface0 for elements, text /
//! subtext0 / overlay1 for the type ladder, blue for the accent, base
//! on accent, green/yellow/red for the status set). The appearance
//! toggle switches the whole flavor — Latte is the set's only light
//! flavor, so every dark flavor resolves to it on `theme: light` —
//! because the style guide forbids blending flavors inside one theme.

use ratatui::style::Color;

/// Which appearance of a palette paints the screen: its dark tokens or
/// its light ones. Parsed from the stored config (`theme: dark|light`),
/// not a flag — a machine choice, not a per-invocation one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThemeKind {
    /// The dark tokens: the primary appearance.
    Dark,
    /// The light tokens, for light terminals.
    Light,
}

/// One palette appearance's tokens, resolved to terminal colors.
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

impl Theme {
    /// Whether the ground is the dark end of its palette: the dark
    /// appearance. The few decisions that need the appearance rather
    /// than a token (the per-profile fallback tones) read it from here,
    /// so generated palettes answer without a side channel.
    pub fn is_dark(&self) -> bool {
        relative_luminance(self.ground) < relative_luminance(self.text)
    }
}

/// WCAG relative luminance. Palette tokens are always `Rgb`; anything
/// else reads as mid gray and lets the ground/text comparison decide.
fn relative_luminance(color: Color) -> f64 {
    fn channel(value: u8) -> f64 {
        let c = f64::from(value) / 255.0;
        if c <= 0.03928 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    }
    match color {
        Color::Rgb(r, g, b) => 0.2126 * channel(r) + 0.7152 * channel(g) + 0.0722 * channel(b),
        _ => 0.5,
    }
}

/// Parses a `#RRGGBB` or `RRGGBB` config color as `0xRRGGBB`.
pub(crate) fn parse_hex_u32(text: &str) -> Option<u32> {
    let hex = text.trim().strip_prefix('#').unwrap_or(text.trim());
    if hex.len() != 6 {
        return None;
    }
    u32::from_str_radix(hex, 16).ok()
}

/// Parses a `#RRGGBB` or `RRGGBB` config color. Config colors are user
/// data, not component literals — rule 1 governs code, not profiles.
pub fn parse_hex_color(text: &str) -> Option<Color> {
    parse_hex_u32(text).map(|value| {
        Color::Rgb(
            (value >> 16) as u8,
            ((value >> 8) & 0xFF) as u8,
            (value & 0xFF) as u8,
        )
    })
}

/// The color a profile name wears when its config sets none: the two
/// seeded profiles get distinct everyday tones, anything else reads
/// secondary.
pub fn default_profile_color(name: &str, theme: &Theme) -> Color {
    match name {
        "personal" => theme.accent_text,
        // Work wears a burnt orange, kin to warn but redder and its own:
        // softened from the #EB7D34 it grew from so its saturation sits
        // with the mid-saturation status set, and darkened on the light
        // appearance to stay readable on the near-white ground.
        "work" => {
            if theme.is_dark() {
                Color::Rgb(0xE2, 0x85, 0x4A)
            } else {
                Color::Rgb(0x9C, 0x52, 0x18)
            }
        }
        _ => theme.text2,
    }
}

/// The dark Rhema appearance: the default.
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

/// The light Rhema appearance, for light-terminal users.
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

/// The dark Aquamarine appearance: the jade tint pushed toward the
/// aqua-cyan side of the hue, with the accent a true aquamarine.
pub const AQUAMARINE_DARK: Theme = Theme {
    ground: Color::Rgb(0x0C, 0x15, 0x18),
    surface: Color::Rgb(0x12, 0x20, 0x23),
    elevated: Color::Rgb(0x19, 0x2A, 0x2E),
    sunken: Color::Rgb(0x07, 0x10, 0x13),
    border: Color::Rgb(0x23, 0x3A, 0x3F),
    border_strong: Color::Rgb(0x38, 0x54, 0x5A),
    text: Color::Rgb(0xED, 0xF5, 0xF4),
    text2: Color::Rgb(0xB6, 0xCB, 0xCB),
    text3: Color::Rgb(0x7F, 0x98, 0x9A),
    accent: Color::Rgb(0x2E, 0x9E, 0x96),
    accent_text: Color::Rgb(0x7C, 0xD9, 0xCB),
    on_accent: Color::Rgb(0x04, 0x12, 0x0F),
    running: Color::Rgb(0x7C, 0xD9, 0xCB),
    ok: Color::Rgb(0x55, 0xC6, 0x95),
    warn: Color::Rgb(0xD9, 0xA6, 0x43),
    error: Color::Rgb(0xE4, 0x68, 0x7A),
    info: Color::Rgb(0x8F, 0xA6, 0xB8),
};

/// The light Aquamarine appearance.
pub const AQUAMARINE_LIGHT: Theme = Theme {
    ground: Color::Rgb(0xF4, 0xFA, 0xF9),
    surface: Color::Rgb(0xFF, 0xFF, 0xFF),
    elevated: Color::Rgb(0xFF, 0xFF, 0xFF),
    sunken: Color::Rgb(0xE4, 0xF0, 0xEE),
    border: Color::Rgb(0xD5, 0xE4, 0xE2),
    border_strong: Color::Rgb(0xB7, 0xCF, 0xCC),
    text: Color::Rgb(0x0C, 0x19, 0x1B),
    text2: Color::Rgb(0x3E, 0x55, 0x58),
    text3: Color::Rgb(0x5A, 0x70, 0x73),
    accent: Color::Rgb(0x0E, 0x6B, 0x63),
    accent_text: Color::Rgb(0x0E, 0x6B, 0x63),
    on_accent: Color::Rgb(0xFF, 0xFF, 0xFF),
    running: Color::Rgb(0x0E, 0x6B, 0x63),
    ok: Color::Rgb(0x14, 0x6B, 0x4E),
    warn: Color::Rgb(0x8A, 0x5D, 0x12),
    error: Color::Rgb(0xB3, 0x35, 0x4A),
    info: Color::Rgb(0x4A, 0x60, 0x70),
};

/// The dark Black-and-white appearance: grayscale only, hue nowhere.
/// The status set carries meaning by luminance alone — a deliberate
/// trade for a palette that promises zero color.
pub const BLACK_WHITE_DARK: Theme = Theme {
    ground: Color::Rgb(0x0A, 0x0A, 0x0A),
    surface: Color::Rgb(0x15, 0x15, 0x15),
    elevated: Color::Rgb(0x1F, 0x1F, 0x1F),
    sunken: Color::Rgb(0x04, 0x04, 0x04),
    border: Color::Rgb(0x27, 0x27, 0x27),
    border_strong: Color::Rgb(0x41, 0x41, 0x41),
    text: Color::Rgb(0xF5, 0xF5, 0xF5),
    text2: Color::Rgb(0xB3, 0xB3, 0xB3),
    text3: Color::Rgb(0x7A, 0x7A, 0x7A),
    accent: Color::Rgb(0xF5, 0xF5, 0xF5),
    accent_text: Color::Rgb(0xF5, 0xF5, 0xF5),
    on_accent: Color::Rgb(0x0A, 0x0A, 0x0A),
    running: Color::Rgb(0xF5, 0xF5, 0xF5),
    ok: Color::Rgb(0xC9, 0xC9, 0xC9),
    warn: Color::Rgb(0x96, 0x96, 0x96),
    error: Color::Rgb(0xFF, 0xFF, 0xFF),
    info: Color::Rgb(0xB3, 0xB3, 0xB3),
};

/// The light Black-and-white appearance.
pub const BLACK_WHITE_LIGHT: Theme = Theme {
    ground: Color::Rgb(0xFA, 0xFA, 0xFA),
    surface: Color::Rgb(0xFF, 0xFF, 0xFF),
    elevated: Color::Rgb(0xFF, 0xFF, 0xFF),
    sunken: Color::Rgb(0xEC, 0xEC, 0xEC),
    border: Color::Rgb(0xDF, 0xDF, 0xDF),
    border_strong: Color::Rgb(0xC4, 0xC4, 0xC4),
    text: Color::Rgb(0x0D, 0x0D, 0x0D),
    text2: Color::Rgb(0x4A, 0x4A, 0x4A),
    text3: Color::Rgb(0x67, 0x67, 0x67),
    accent: Color::Rgb(0x0D, 0x0D, 0x0D),
    accent_text: Color::Rgb(0x0D, 0x0D, 0x0D),
    on_accent: Color::Rgb(0xFF, 0xFF, 0xFF),
    running: Color::Rgb(0x0D, 0x0D, 0x0D),
    ok: Color::Rgb(0x3D, 0x3D, 0x3D),
    warn: Color::Rgb(0x6F, 0x6F, 0x6F),
    error: Color::Rgb(0x00, 0x00, 0x00),
    info: Color::Rgb(0x4A, 0x4A, 0x4A),
};

/// The dark Dracula appearance: the official palette, mapped onto the
/// token slots (purple accent, pink in flight, cyan notices).
pub const DRACULA_DARK: Theme = Theme {
    ground: Color::Rgb(0x21, 0x22, 0x2C),
    surface: Color::Rgb(0x28, 0x2A, 0x36),
    elevated: Color::Rgb(0x34, 0x37, 0x46),
    sunken: Color::Rgb(0x19, 0x1A, 0x21),
    border: Color::Rgb(0x44, 0x47, 0x5A),
    border_strong: Color::Rgb(0x62, 0x72, 0xA4),
    text: Color::Rgb(0xF8, 0xF8, 0xF2),
    text2: Color::Rgb(0xA3, 0xA9, 0xC9),
    text3: Color::Rgb(0x62, 0x72, 0xA4),
    accent: Color::Rgb(0xBD, 0x93, 0xF9),
    accent_text: Color::Rgb(0xBD, 0x93, 0xF9),
    on_accent: Color::Rgb(0x21, 0x22, 0x2C),
    running: Color::Rgb(0xFF, 0x79, 0xC6),
    ok: Color::Rgb(0x50, 0xFA, 0x7B),
    warn: Color::Rgb(0xFF, 0xB8, 0x6C),
    error: Color::Rgb(0xFF, 0x55, 0x55),
    info: Color::Rgb(0x8B, 0xE9, 0xFD),
};

/// The light Dracula appearance. Dracula ships no official light
/// variant; these keep the official hues, darkened where text-level
/// contrast on a near-white ground demands it.
pub const DRACULA_LIGHT: Theme = Theme {
    ground: Color::Rgb(0xF7, 0xF7, 0xF1),
    surface: Color::Rgb(0xFF, 0xFF, 0xFF),
    elevated: Color::Rgb(0xFF, 0xFF, 0xFF),
    sunken: Color::Rgb(0xEB, 0xEB, 0xE4),
    border: Color::Rgb(0xDB, 0xDC, 0xE5),
    border_strong: Color::Rgb(0xB4, 0xB9, 0xD4),
    text: Color::Rgb(0x21, 0x22, 0x2C),
    text2: Color::Rgb(0x4C, 0x51, 0x70),
    text3: Color::Rgb(0x62, 0x72, 0xA4),
    accent: Color::Rgb(0x68, 0x42, 0xB8),
    accent_text: Color::Rgb(0x68, 0x42, 0xB8),
    on_accent: Color::Rgb(0xFF, 0xFF, 0xFF),
    running: Color::Rgb(0xC4, 0x2B, 0x93),
    ok: Color::Rgb(0x18, 0x99, 0x4E),
    warn: Color::Rgb(0xA1, 0x5E, 0x08),
    error: Color::Rgb(0xD3, 0x25, 0x41),
    info: Color::Rgb(0x0E, 0x7D, 0x95),
};

/// The Catppuccin Frappé appearance: the official flavor hexes in the
/// style guide's roles.
pub const CATPPUCCIN_FRAPPE: Theme = Theme {
    ground: Color::Rgb(0x30, 0x34, 0x46),
    surface: Color::Rgb(0x29, 0x2C, 0x3C),
    elevated: Color::Rgb(0x41, 0x45, 0x59),
    sunken: Color::Rgb(0x23, 0x26, 0x34),
    border: Color::Rgb(0x51, 0x57, 0x6D),
    border_strong: Color::Rgb(0x73, 0x79, 0x94),
    text: Color::Rgb(0xC6, 0xD0, 0xF5),
    text2: Color::Rgb(0xA5, 0xAD, 0xCE),
    text3: Color::Rgb(0x83, 0x8B, 0xA7),
    accent: Color::Rgb(0x8C, 0xAA, 0xEE),
    accent_text: Color::Rgb(0x8C, 0xAA, 0xEE),
    on_accent: Color::Rgb(0x30, 0x34, 0x46),
    running: Color::Rgb(0x8C, 0xAA, 0xEE),
    ok: Color::Rgb(0xA6, 0xD1, 0x89),
    warn: Color::Rgb(0xE5, 0xC8, 0x90),
    error: Color::Rgb(0xE7, 0x82, 0x84),
    info: Color::Rgb(0x85, 0xC1, 0xDC),
};

/// The Catppuccin Macchiato appearance: the official flavor hexes in
/// the style guide's roles.
pub const CATPPUCCIN_MACCHIATO: Theme = Theme {
    ground: Color::Rgb(0x24, 0x27, 0x3A),
    surface: Color::Rgb(0x1E, 0x20, 0x30),
    elevated: Color::Rgb(0x36, 0x3A, 0x4F),
    sunken: Color::Rgb(0x18, 0x19, 0x26),
    border: Color::Rgb(0x49, 0x4D, 0x64),
    border_strong: Color::Rgb(0x6E, 0x73, 0x8D),
    text: Color::Rgb(0xCA, 0xD3, 0xF5),
    text2: Color::Rgb(0xA5, 0xAD, 0xCB),
    text3: Color::Rgb(0x80, 0x87, 0xA2),
    accent: Color::Rgb(0x8A, 0xAD, 0xF4),
    accent_text: Color::Rgb(0x8A, 0xAD, 0xF4),
    on_accent: Color::Rgb(0x24, 0x27, 0x3A),
    running: Color::Rgb(0x8A, 0xAD, 0xF4),
    ok: Color::Rgb(0xA6, 0xDA, 0x95),
    warn: Color::Rgb(0xEE, 0xD4, 0x9F),
    error: Color::Rgb(0xED, 0x87, 0x96),
    info: Color::Rgb(0x7D, 0xC4, 0xE4),
};

/// The Catppuccin Mocha appearance: the official flavor hexes in the
/// style guide's roles.
pub const CATPPUCCIN_MOCHA: Theme = Theme {
    ground: Color::Rgb(0x1E, 0x1E, 0x2E),
    surface: Color::Rgb(0x18, 0x18, 0x25),
    elevated: Color::Rgb(0x31, 0x32, 0x44),
    sunken: Color::Rgb(0x11, 0x11, 0x1B),
    border: Color::Rgb(0x45, 0x47, 0x5A),
    border_strong: Color::Rgb(0x6C, 0x70, 0x86),
    text: Color::Rgb(0xCD, 0xD6, 0xF4),
    text2: Color::Rgb(0xA6, 0xAD, 0xC8),
    text3: Color::Rgb(0x7F, 0x84, 0x9C),
    accent: Color::Rgb(0x89, 0xB4, 0xFA),
    accent_text: Color::Rgb(0x89, 0xB4, 0xFA),
    on_accent: Color::Rgb(0x1E, 0x1E, 0x2E),
    running: Color::Rgb(0x89, 0xB4, 0xFA),
    ok: Color::Rgb(0xA6, 0xE3, 0xA1),
    warn: Color::Rgb(0xF9, 0xE2, 0xAF),
    error: Color::Rgb(0xF3, 0x8B, 0xA8),
    info: Color::Rgb(0x74, 0xC7, 0xEC),
};

/// The Catppuccin Latte appearance: the light side of every Catppuccin
/// palette here. The style guide's roles apply as on the dark flavors,
/// with three documented deviations where the official Latte hexes sit
/// under the contrast floors this crate enforces ("legibility always
/// comes first", the guide's own escape hatch): hints read overlay2
/// rather than overlay1 (2.8:1 on base), and the green/yellow/sapphire
/// status accents are darkened just past 3:1 (#35911F, #A1720F,
/// #147A8C). The guide's "on accent: base" lands at 4.3:1 on blue, so
/// the label reads white like the other light sides.
pub const CATPPUCCIN_LATTE: Theme = Theme {
    ground: Color::Rgb(0xEF, 0xF1, 0xF5),
    surface: Color::Rgb(0xE6, 0xE9, 0xEF),
    elevated: Color::Rgb(0xCC, 0xD0, 0xDA),
    sunken: Color::Rgb(0xDC, 0xE0, 0xE8),
    border: Color::Rgb(0xBC, 0xC0, 0xCC),
    border_strong: Color::Rgb(0x9C, 0xA0, 0xB0),
    text: Color::Rgb(0x4C, 0x4F, 0x69),
    text2: Color::Rgb(0x6C, 0x6F, 0x85),
    text3: Color::Rgb(0x7C, 0x7F, 0x93),
    accent: Color::Rgb(0x1E, 0x66, 0xF5),
    accent_text: Color::Rgb(0x1E, 0x66, 0xF5),
    on_accent: Color::Rgb(0xFF, 0xFF, 0xFF),
    running: Color::Rgb(0x1E, 0x66, 0xF5),
    ok: Color::Rgb(0x35, 0x91, 0x1F),
    warn: Color::Rgb(0xA1, 0x72, 0x0F),
    error: Color::Rgb(0xD2, 0x0F, 0x39),
    info: Color::Rgb(0x14, 0x7A, 0x8C),
};

/// A named color palette: one family of tokens, each in a dark and a
/// light appearance. The stored config names one (`colors: rhema`);
/// anything unparseable reads as the default, like the other
/// look-and-feel keys.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Palette {
    /// The Rhema palette (Jadeite): the default.
    #[default]
    Rhema,
    /// The Aquamarine palette.
    Aquamarine,
    /// The monochrome palette.
    BlackWhite,
    /// The Dracula palette.
    Dracula,
    /// The Catppuccin Frappé palette; light reads Latte.
    CatppuccinFrappe,
    /// The Catppuccin Macchiato palette; light reads Latte.
    CatppuccinMacchiato,
    /// The Catppuccin Mocha palette; light reads Latte.
    CatppuccinMocha,
    /// The M3 You palette: a Material 3 tonal-spot scheme generated
    /// from a seed color (`m3_source`, defaulting to Material's
    /// baseline purple).
    M3You,
}

impl Palette {
    /// Every palette, in the settings cycle order.
    pub const ALL: &'static [Palette] = &[
        Palette::Rhema,
        Palette::Aquamarine,
        Palette::BlackWhite,
        Palette::Dracula,
        Palette::CatppuccinFrappe,
        Palette::CatppuccinMacchiato,
        Palette::CatppuccinMocha,
        Palette::M3You,
    ];

    /// Parses a stored config value; `None` for no value and unknown
    /// spellings alike — the caller decides whether that means the
    /// default palette or an error naming the stored value.
    pub fn from_stored(value: Option<&str>) -> Option<Self> {
        match value {
            Some("rhema") => Some(Self::Rhema),
            Some("aquamarine") => Some(Self::Aquamarine),
            Some("black-and-white") | Some("black_and_white") | Some("blackandwhite") => {
                Some(Self::BlackWhite)
            }
            Some("dracula") => Some(Self::Dracula),
            Some("catppuccin-frappe") | Some("catppuccin_frappe") | Some("catppuccinfrappe") => {
                Some(Self::CatppuccinFrappe)
            }
            Some("catppuccin-macchiato")
            | Some("catppuccin_macchiato")
            | Some("catppuccinmacchiato") => Some(Self::CatppuccinMacchiato),
            Some("catppuccin-mocha") | Some("catppuccin_mocha") | Some("catppuccinmocha") => {
                Some(Self::CatppuccinMocha)
            }
            Some("m3-you") | Some("m3_you") | Some("m3you") | Some("material-you")
            | Some("material_you") => Some(Self::M3You),
            _ => None,
        }
    }

    /// The config value this palette persists as — the inverse of
    /// [`Self::from_stored`] for the canonical spelling.
    pub fn stored_name(self) -> &'static str {
        match self {
            Self::Rhema => "rhema",
            Self::Aquamarine => "aquamarine",
            Self::BlackWhite => "black-and-white",
            Self::Dracula => "dracula",
            Self::CatppuccinFrappe => "catppuccin-frappe",
            Self::CatppuccinMacchiato => "catppuccin-macchiato",
            Self::CatppuccinMocha => "catppuccin-mocha",
            Self::M3You => "m3-you",
        }
    }

    /// The label the settings screen draws.
    pub fn label(self) -> &'static str {
        match self {
            Self::Rhema => "Rhema",
            Self::Aquamarine => "Aquamarine",
            Self::BlackWhite => "Black and white",
            Self::Dracula => "Dracula",
            Self::CatppuccinFrappe => "Catppuccin Frapp\u{e9}",
            Self::CatppuccinMacchiato => "Catppuccin Macchiato",
            Self::CatppuccinMocha => "Catppuccin Mocha",
            Self::M3You => "M3 You",
        }
    }

    /// Resolves the palette's tokens for `kind`. `m3_source` feeds
    /// [`Palette::M3You`] alone (`0xRRGGBB`; `None` is its default
    /// seed) and the fixed palettes ignore it.
    pub fn theme(self, kind: ThemeKind, m3_source: Option<u32>) -> Theme {
        match self {
            Self::Rhema => match kind {
                ThemeKind::Dark => DARK,
                ThemeKind::Light => LIGHT,
            },
            Self::Aquamarine => match kind {
                ThemeKind::Dark => AQUAMARINE_DARK,
                ThemeKind::Light => AQUAMARINE_LIGHT,
            },
            Self::BlackWhite => match kind {
                ThemeKind::Dark => BLACK_WHITE_DARK,
                ThemeKind::Light => BLACK_WHITE_LIGHT,
            },
            Self::Dracula => match kind {
                ThemeKind::Dark => DRACULA_DARK,
                ThemeKind::Light => DRACULA_LIGHT,
            },
            Self::CatppuccinFrappe => match kind {
                ThemeKind::Dark => CATPPUCCIN_FRAPPE,
                ThemeKind::Light => CATPPUCCIN_LATTE,
            },
            Self::CatppuccinMacchiato => match kind {
                ThemeKind::Dark => CATPPUCCIN_MACCHIATO,
                ThemeKind::Light => CATPPUCCIN_LATTE,
            },
            Self::CatppuccinMocha => match kind {
                ThemeKind::Dark => CATPPUCCIN_MOCHA,
                ThemeKind::Light => CATPPUCCIN_LATTE,
            },
            Self::M3You => crate::m3::theme(m3_source.unwrap_or(crate::m3::DEFAULT_SOURCE), kind),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// WCAG contrast ratio between two token colors.
    fn contrast(a: Color, b: Color) -> f64 {
        let (la, lb) = (relative_luminance(a), relative_luminance(b));
        let (hi, lo) = if la >= lb { (la, lb) } else { (lb, la) };
        (hi + 0.05) / (lo + 0.05)
    }

    #[test]
    fn the_seeded_profiles_wear_distinct_theme_tuned_tones() {
        // Personal rides the accent.
        assert_eq!(default_profile_color("personal", &DARK), DARK.accent_text);
        // Work wears its own orange, per appearance: kin to warn but not
        // warn, deeper on the light ground.
        assert_eq!(
            default_profile_color("work", &DARK),
            Color::Rgb(0xE2, 0x85, 0x4A)
        );
        assert_ne!(default_profile_color("work", &DARK), DARK.warn);
        assert_eq!(
            default_profile_color("work", &LIGHT),
            Color::Rgb(0x9C, 0x52, 0x18)
        );
        // Anything else reads secondary.
        assert_eq!(default_profile_color("other", &DARK), DARK.text2);
    }

    #[test]
    fn the_appearance_reads_off_the_tokens() {
        assert!(DARK.is_dark());
        assert!(!LIGHT.is_dark());
        for palette in Palette::ALL {
            assert!(palette.theme(ThemeKind::Dark, None).is_dark());
            assert!(!palette.theme(ThemeKind::Light, None).is_dark());
        }
    }

    #[test]
    fn stored_spellings_round_trip() {
        for palette in Palette::ALL {
            assert_eq!(
                Palette::from_stored(Some(palette.stored_name())),
                Some(*palette)
            );
        }
        // Alias spellings land on the same palette.
        assert_eq!(
            Palette::from_stored(Some("black_and_white")),
            Some(Palette::BlackWhite)
        );
        assert_eq!(
            Palette::from_stored(Some("material_you")),
            Some(Palette::M3You)
        );
        assert_eq!(
            Palette::from_stored(Some("blackandwhite")),
            Some(Palette::BlackWhite)
        );
        // Nothing stored and a spelling no palette answers to both read
        // as unset: the caller applies the default.
        assert_eq!(Palette::from_stored(None), None);
        assert_eq!(Palette::from_stored(Some("nonsense")), None);
    }

    #[test]
    fn every_palette_resolves_both_appearances() {
        for palette in Palette::ALL {
            for kind in [ThemeKind::Dark, ThemeKind::Light] {
                let theme = palette.theme(kind, None);
                assert_ne!(theme.ground, theme.text, "{palette:?} {kind:?}");
            }
        }
    }

    #[test]
    fn the_m3_seed_moves_the_tokens() {
        let default = Palette::M3You.theme(ThemeKind::Dark, None);
        let blue = Palette::M3You.theme(ThemeKind::Dark, Some(0x4285F4));
        assert_eq!(
            default.ground,
            crate::m3::theme(crate::m3::DEFAULT_SOURCE, ThemeKind::Dark).ground
        );
        assert_ne!(default.accent, blue.accent);
        // The fixed palettes ignore the seed.
        assert_eq!(Palette::Dracula.theme(ThemeKind::Dark, None), DRACULA_DARK);
        assert_eq!(
            Palette::Dracula.theme(ThemeKind::Dark, Some(0x4285F4)),
            DRACULA_DARK
        );
    }

    #[test]
    fn tokens_read_on_the_ground_in_every_palette() {
        for palette in Palette::ALL {
            for kind in [ThemeKind::Dark, ThemeKind::Light] {
                let theme = palette.theme(kind, None);
                let label = format!("{palette:?} {kind:?}");
                assert!(contrast(theme.text, theme.ground) >= 4.5, "{label}");
                assert!(contrast(theme.text, theme.surface) >= 4.5, "{label}");
                assert!(contrast(theme.text2, theme.ground) >= 4.0, "{label}");
                assert!(contrast(theme.text3, theme.ground) >= 3.0, "{label}");
                assert!(contrast(theme.accent_text, theme.ground) >= 3.0, "{label}");
                assert!(contrast(theme.on_accent, theme.accent) >= 4.5, "{label}");
                for status in [theme.ok, theme.warn, theme.error, theme.info] {
                    assert!(contrast(status, theme.ground) >= 3.0, "{label}");
                }
            }
        }
    }
}
