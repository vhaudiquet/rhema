//! Terminal graphics for the onboarding logo: the kitty graphics
//! protocol, which Ghostty speaks too.
//!
//! Placement happens OUTSIDE the frame: ratatui paints cells, and a
//! graphics-protocol image is a separate layer composited above them,
//! so drawing a frame never erases one. [`place`] is called after a
//! draw, with the terminal cursor moved to the cell the image anchors
//! on; the caller re-places when the anchor rect moves (a resize) and
//! calls [`clear`] when the image's screen goes away. Leaving the
//! alternate screen discards the images in conforming terminals, but
//! `clear` runs on teardown anyway — a terminal that keeps them must
//! not show a stray logo over the user's shell.
//!
//! The one escape used is transmit-and-display (`a=T`): the PNG payload
//! rides the escape, and `c`/`r` scale it to the cell area given. The
//! payload is chunked (`m=1`) at the protocol's recommended chunk size,
//! so a 45 KB logo crosses in a handful of writes a pipe absorbs
//! instantly.

use std::io::{IsTerminal, Write};

/// The PNG the binary carries, copied from the repository's images by
/// `build.rs` — empty in a packaged build without the asset, where
/// [`Self::logo_png`] answers `None` and the caller falls back to the
/// text wordmark.
const LOGO_PNG: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/rhema-logo.png"));

/// The logo asset, when the binary carries one.
pub fn logo_png() -> Option<&'static [u8]> {
    (!LOGO_PNG.is_empty()).then_some(LOGO_PNG)
}

/// The base64 chunk the kitty protocol recommends: larger chunks are
/// legal but may overflow terminal input buffers.
const CHUNK: usize = 4096;

/// Whether this terminal draws the kitty graphics protocol — kitty and
/// Ghostty, by their own environment marks — and an image to draw
/// exists. `RHEMA_NO_IMAGES` overrides detection off: an escape a
/// terminal mislabels as garbage on screen is worse than no logo.
pub fn supported() -> bool {
    if std::env::var("RHEMA_NO_IMAGES").is_ok() || logo_png().is_none() {
        return false;
    }
    if !std::io::stdout().is_terminal() {
        return false;
    }
    let kitty = std::env::var_os("KITTY_WINDOW_ID").is_some()
        || std::env::var_os("KITTY_PID").is_some()
        || std::env::var("TERM").as_deref() == Ok("xterm-kitty");
    let ghostty = std::env::var_os("GHOSTTY_RESOURCES_DIR").is_some()
        || std::env::var("TERM_PROGRAM").as_deref() == Ok("ghostty");
    kitty || ghostty
}

/// Deletes every image this program placed. Safe on terminals without
/// the protocol: an unknown APC sequence is ignored by design.
pub fn clear() {
    let mut out = std::io::stdout();
    let _ = out.write_all(b"\x1b_Ga=d,d=a,q=2\x1b\\");
    let _ = out.flush();
}

/// Places the logo at `x`/`y` (cell column/row, zero-based), scaled to
/// `cols` cells wide and `rows` cells tall. Any image placed before is
/// deleted first, so a re-place never stacks two logos. Failures are
/// swallowed: a logo that cannot draw is a missing decoration, not an
/// error worth a screen.
pub fn place(x: u16, y: u16, cols: u16, rows: u16) {
    let Some(png) = logo_png() else { return };
    let encoded = base64(png);
    let mut out = std::io::stdout();
    // The image anchors on the cell the terminal cursor sits at when
    // the escape lands; the cursor itself does not move.
    let _ = out.write_all(format!("\x1b[{};{}H", y + 1, x + 1).as_bytes());
    clear();
    let total = encoded.len();
    let mut offset = 0;
    while offset < total {
        let end = (offset + CHUNK).min(total);
        let more = end < total;
        let head = if offset == 0 {
            format!("a=T,f=100,c={cols},r={rows},q=2")
        } else {
            "a=T,q=2".to_owned()
        };
        let flags = if more { ",m=1" } else { "" };
        let _ = out.write_all(b"\x1b_G");
        let _ = out.write_all(head.as_bytes());
        let _ = out.write_all(flags.as_bytes());
        let _ = out.write_all(b";");
        let _ = out.write_all(&encoded.as_bytes()[offset..end]);
        let _ = out.write_all(b"\x1b\\");
        offset = end;
    }
    let _ = out.flush();
}

/// Standard base64 — the graphics protocol's payload alphabet. Written
/// here rather than pulled in as a dependency: one table, one padding
/// rule, no configuration.
fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let byte = |i: usize| chunk.get(i).copied().unwrap_or(0) as u32;
        let group = (byte(0) << 16) | (byte(1) << 8) | byte(2);
        out.push(ALPHABET[(group >> 18) as usize & 0x3F] as char);
        out.push(ALPHABET[(group >> 12) as usize & 0x3F] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[(group >> 6) as usize & 0x3F] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[group as usize & 0x3F] as char
        } else {
            '='
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_the_reference_vectors() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foob"), "Zm9vYg==");
        assert_eq!(base64(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn the_embedded_payload_is_a_png_or_none() {
        if let Some(bytes) = logo_png() {
            assert!(bytes.len() > 8, "a real asset, not a stray file");
            assert_eq!(&bytes[..4], b"\x89PNG", "the PNG magic");
        }
    }
}
