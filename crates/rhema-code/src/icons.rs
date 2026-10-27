//! The footer's glyphs: one place for every icon the UI draws, in two
//! flavors — plain Unicode that renders in any terminal font, and Nerd
//! Font glyphs for users who carry the font. Nothing else in the tree
//! hardcodes an icon.

/// The icon set the footer draws with.
#[derive(Debug, Clone, Copy)]
pub struct Icons {
    /// The thinking-effort badge.
    pub thinking: &'static str,
    /// The context gauge for a fill fraction in `0.0..=1.0`.
    pub context: fn(fraction: f64) -> &'static str,
    /// The current-folder badge.
    pub folder: &'static str,
    /// The subscription-allowance badge.
    pub allowance: &'static str,
    /// The settings badge.
    pub settings: &'static str,
    /// The subagent badge — a delegated worker's line.
    pub robot: &'static str,
    /// The vision badge — a model that accepts image input.
    pub vision: &'static str,
}

/// The plain set: every glyph renders in any terminal font. The thinking
/// badge is the typographic asterism — a break for reflection; the folder
/// badge is an open tray; the allowance badge the generic currency mark;
/// the settings badge a gear pinned to its text presentation so it never
/// arrives as color emoji; the vision badge the fisheye, an eye any font
/// draws. No plain font carries a robot, so the subagent badge is the
/// delegation sign: `@` — work handed to another.
pub const UNICODE: Icons = Icons {
    thinking: "⁂",
    context: unicode_gauge,
    folder: "❐",
    allowance: "¤",
    settings: "\u{2699}\u{fe0e}",
    robot: "@",
    vision: "◉",
};

/// The Nerd Font set: FontAwesome lightbulb, database, folder, credit
/// card, and eye — needs a patched font installed, or the badges read
/// as tofu. Every glyph sits in the FA core range (F000–F2E0) the
/// oldest patches carry; the brain this set once used sits where Nerd
/// Fonts v3 dropped Material Design, and v3 fonts render nothing
/// there. The robot is the one exception: the FA5 block (F544) the
/// common v3 patched fonts still carry, because the FA core range has
/// no robot.
pub const NERD: Icons = Icons {
    thinking: "\u{f0eb}",
    context: nerd_gauge,
    folder: "\u{f07b}",
    allowance: "\u{f09d}",
    settings: "\u{f013}",
    robot: "\u{f544}",
    vision: "\u{f06e}",
};

/// Which flavor the footer draws with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum)]
pub enum Flavor {
    /// Plain Unicode; renders everywhere.
    #[default]
    Unicode,
    /// Nerd Font glyphs.
    Nerd,
}

impl Flavor {
    /// Parses a flavor name; `None` for anything else.
    pub fn parse(text: &str) -> Option<Self> {
        match text.trim().to_lowercase().as_str() {
            "unicode" => Some(Self::Unicode),
            "nerd" => Some(Self::Nerd),
            _ => None,
        }
    }

    /// The icon set this flavor draws with.
    pub fn set(self) -> &'static Icons {
        match self {
            Self::Unicode => &UNICODE,
            Self::Nerd => &NERD,
        }
    }
}

/// A quarter-stepped fill gauge: the glyph itself shows how full the
/// context window is, readable before the numbers are.
fn unicode_gauge(fraction: f64) -> &'static str {
    match fraction {
        // Garbage input reads as empty, never as an alarming full.
        f if !f.is_finite() || f < 0.25 => "◔",
        f if f < 0.5 => "◑",
        f if f < 0.75 => "◕",
        _ => "●",
    }
}

/// The database glyph, whatever the fill: Nerd Font glyphs do not ship a
/// fill family.
fn nerd_gauge(_fraction: f64) -> &'static str {
    "\u{f1c0}"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_unicode_gauge_fills_in_quarters() {
        assert_eq!(unicode_gauge(0.0), "◔");
        assert_eq!(unicode_gauge(0.24), "◔");
        assert_eq!(unicode_gauge(0.25), "◑");
        assert_eq!(unicode_gauge(0.5), "◕");
        assert_eq!(unicode_gauge(0.9), "●");
        // Out-of-range fractions degrade to the emptiest glyph.
        assert_eq!(unicode_gauge(-1.0), "◔");
        assert_eq!(unicode_gauge(f64::NAN), "◔");
    }

    #[test]
    fn flavors_parse_and_resolve() {
        assert_eq!(Flavor::parse(" nerd "), Some(Flavor::Nerd));
        assert_eq!(Flavor::parse("emoji"), None);
        assert_eq!(Flavor::default(), Flavor::Unicode);
        assert_eq!(Flavor::Nerd.set().thinking, "\u{f0eb}");
        assert_eq!(Flavor::Unicode.set().thinking, "⁂");
        // Both sets name the allowance with a money glyph…
        assert_eq!(Flavor::Unicode.set().allowance, "¤");
        assert_eq!(Flavor::Nerd.set().allowance, "\u{f09d}");
        // …and the settings screen with a gear.
        assert_eq!(Flavor::Unicode.set().settings, "\u{2699}\u{fe0e}");
        assert_eq!(Flavor::Nerd.set().settings, "\u{f013}");
        // The vision badge is an eye in both.
        assert_eq!(Flavor::Unicode.set().vision, "◉");
        assert_eq!(Flavor::Nerd.set().vision, "\u{f06e}");
    }
}
