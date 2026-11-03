//! The M3 You palette: a Material 3 tonal-spot scheme generated from a
//! seed color at startup. The seed's hue drives five tonal palettes at
//! the fixed tonal-spot chromas (primary 36, tertiary hue+60 at 24,
//! neutral 4, neutral-variant 8, error 25/84); tones map onto the
//! Jadeite token slots — background/surface-container tones for the
//! layers, primary for the accent, the standard error palette, and
//! fixed-hue green/amber palettes for ok/warn the way M3 fixes error.
//!
//! A terminal cannot see the wallpaper; the seed comes from the config
//! (`m3_source`), and Material's baseline purple stands in when unset.

use super::hct::{Hct, TonalPalette};
use crate::palette::{Theme, ThemeKind};
use ratatui::style::Color;

/// The seed when the config names none: Material's baseline purple.
pub(crate) const DEFAULT_SOURCE: u32 = 0x67_50_A4;

fn rgb(argb: u32) -> Color {
    Color::Rgb(
        ((argb >> 16) & 0xFF) as u8,
        ((argb >> 8) & 0xFF) as u8,
        (argb & 0xFF) as u8,
    )
}

/// The tonal-spot tokens for `source` (an `0xRRGGBB` seed) in `kind`.
pub(crate) fn theme(source: u32, kind: ThemeKind) -> Theme {
    let dark = kind == ThemeKind::Dark;
    let hue = Hct::from_argb(0xFF00_0000 | (source & 0x00FF_FFFF)).hue;
    let primary = TonalPalette::new(hue, 36.0);
    let tertiary = TonalPalette::new(sanitize_degrees(hue + 60.0), 24.0);
    let neutral = TonalPalette::new(hue, 4.0);
    let neutral_variant = TonalPalette::new(hue, 8.0);
    let error = TonalPalette::new(25.0, 84.0);
    let green = TonalPalette::new(140.0, 40.0);
    let amber = TonalPalette::new(75.0, 55.0);
    let tone = |palette: &TonalPalette, dark_tone: u8, light_tone: u8| {
        rgb(palette.tone(if dark { dark_tone } else { light_tone }))
    };
    Theme {
        // The surface-container ladder: the canvas sits at background,
        // the layers rise through the containers, code sinks below it.
        ground: tone(&neutral, 6, 98),
        surface: tone(&neutral, 10, 96),
        elevated: tone(&neutral, 17, 92),
        sunken: tone(&neutral, 4, 94),
        // outlineVariant then outline: the hairline reads softer than
        // the divider that must.
        border: tone(&neutral_variant, 30, 80),
        border_strong: tone(&neutral_variant, 60, 50),
        text: tone(&neutral, 90, 10),
        text2: tone(&neutral_variant, 80, 30),
        // The hint floor sits between onSurfaceVariant and outline.
        text3: tone(&neutral_variant, 50, 45),
        accent: tone(&primary, 80, 40),
        accent_text: tone(&primary, 80, 40),
        on_accent: tone(&primary, 20, 100),
        running: tone(&primary, 80, 40),
        ok: tone(&green, 80, 40),
        warn: tone(&amber, 80, 40),
        error: tone(&error, 80, 40),
        info: tone(&tertiary, 80, 40),
    }
}

fn sanitize_degrees(degrees: f64) -> f64 {
    let degrees = degrees % 360.0;
    if degrees < 0.0 {
        degrees + 360.0
    } else {
        degrees
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_baseline_seed_matches_the_reference_scheme() {
        // Values generated from Google's material-color-utilities
        // (2021 spec, tonal spot, contrast 0) for the same tones.
        let dark = theme(0x6750A4, ThemeKind::Dark);
        assert_eq!(dark.ground, Color::Rgb(0x14, 0x13, 0x16));
        assert_eq!(dark.surface, Color::Rgb(0x1C, 0x1B, 0x1E));
        assert_eq!(dark.elevated, Color::Rgb(0x2B, 0x29, 0x2D));
        assert_eq!(dark.sunken, Color::Rgb(0x0F, 0x0E, 0x11));
        assert_eq!(dark.border, Color::Rgb(0x49, 0x45, 0x4E));
        assert_eq!(dark.border_strong, Color::Rgb(0x94, 0x8F, 0x99));
        assert_eq!(dark.text, Color::Rgb(0xE6, 0xE1, 0xE6));
        assert_eq!(dark.text2, Color::Rgb(0xCA, 0xC4, 0xCF));
        assert_eq!(dark.text3, Color::Rgb(0x7A, 0x75, 0x7F));
        assert_eq!(dark.accent, Color::Rgb(0xCF, 0xBD, 0xFE));
        assert_eq!(dark.on_accent, Color::Rgb(0x36, 0x27, 0x5D));
        assert_eq!(dark.ok, Color::Rgb(0xA3, 0xD4, 0x8D));
        assert_eq!(dark.warn, Color::Rgb(0xFF, 0xB9, 0x4E));
        assert_eq!(dark.error, Color::Rgb(0xFF, 0xB4, 0xAB));
        assert_eq!(dark.info, Color::Rgb(0xEF, 0xB8, 0xC8));
    }

    #[test]
    fn the_light_appearance_matches_the_reference_scheme() {
        let light = theme(0x6750A4, ThemeKind::Light);
        assert_eq!(light.ground, Color::Rgb(0xFD, 0xF8, 0xFD));
        assert_eq!(light.surface, Color::Rgb(0xF7, 0xF2, 0xF7));
        assert_eq!(light.elevated, Color::Rgb(0xEC, 0xE7, 0xEB));
        assert_eq!(light.sunken, Color::Rgb(0xF2, 0xEC, 0xF1));
        assert_eq!(light.border, Color::Rgb(0xCA, 0xC4, 0xCF));
        assert_eq!(light.border_strong, Color::Rgb(0x7A, 0x75, 0x7F));
        assert_eq!(light.text, Color::Rgb(0x1C, 0x1B, 0x1E));
        assert_eq!(light.text2, Color::Rgb(0x49, 0x45, 0x4E));
        assert_eq!(light.text3, Color::Rgb(0x6D, 0x69, 0x73));
        assert_eq!(light.accent, Color::Rgb(0x65, 0x55, 0x8F));
        assert_eq!(light.on_accent, Color::Rgb(0xFF, 0xFF, 0xFF));
        assert_eq!(light.ok, Color::Rgb(0x3E, 0x69, 0x2E));
        assert_eq!(light.warn, Color::Rgb(0x82, 0x55, 0x00));
        assert_eq!(light.error, Color::Rgb(0xBA, 0x1A, 0x1A));
        assert_eq!(light.info, Color::Rgb(0x7E, 0x52, 0x60));
    }

    #[test]
    fn the_seed_moves_the_accent() {
        let blue = theme(0x4285F4, ThemeKind::Dark);
        assert_eq!(blue.accent, Color::Rgb(0xAD, 0xC6, 0xFF));
        assert_eq!(blue.on_accent, Color::Rgb(0x10, 0x2F, 0x60));
        assert_eq!(blue.ground, Color::Rgb(0x12, 0x13, 0x16));
    }

    #[test]
    fn generation_is_deterministic() {
        assert_eq!(
            theme(0x4285F4, ThemeKind::Dark),
            theme(0x4285F4, ThemeKind::Dark)
        );
        assert_ne!(
            theme(0x4285F4, ThemeKind::Dark),
            theme(0x4285F4, ThemeKind::Light)
        );
    }
}
