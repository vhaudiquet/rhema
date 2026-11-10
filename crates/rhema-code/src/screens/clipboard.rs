//! The system clipboard: ctrl-v as a paste key where the terminal does
//! not bind it itself, and the transcript selection's copy. A fresh
//! handle per use — the connection is not held open between presses.
//!
//! Desktop sessions write through `arboard`; a terminal-only session
//! (a pipe, an ssh hop, a TTY with no X11 or Wayland display) has no
//! clipboard to connect to, so the copy falls back to OSC 52 and lets
//! the terminal put the text on its own clipboard.

use base64::Engine;
use std::io::Write as _;

/// Writes `text` to the clipboard, best effort. A reachable desktop
/// clipboard takes it directly; otherwise the terminal is asked with
/// OSC 52, which is also what carries a copy over ssh. The terminal
/// silently ignores the sequence when it does not support it, so there
/// is no failure to report.
pub fn write(text: &str) {
    if arboard::Clipboard::new()
        .and_then(|mut clipboard| clipboard.set_text(text))
        .is_ok()
    {
        return;
    }
    let mut stdout = std::io::stdout().lock();
    let _ = stdout
        .write_all(osc52(text).as_bytes())
        .and_then(|()| stdout.flush());
}

/// The OSC 52 sequence that asks the terminal to put `text` on its
/// clipboard: the payload is the UTF-8 bytes as base64.
fn osc52(text: &str) -> String {
    format!(
        "\x1b]52;c;{}\x1b\\",
        base64::engine::general_purpose::STANDARD.encode(text)
    )
}

/// The clipboard's text, when one is reachable.
pub fn read() -> Option<String> {
    arboard::Clipboard::new()
        .and_then(|mut clipboard| clipboard.get_text())
        .ok()
        .filter(|text| !text.is_empty())
}

/// The clipboard's image, when one is reachable: the RGBA pixels are
/// re-encoded as PNG — the one format every provider accepts and the
/// neutral carrier for whatever the copy came from — and travel as bare
/// base64, the form [`rhema_core::Image`] carries. Missing or failed
/// reads are `None`, exactly like [`read`]: a clipboard that holds text,
/// or nothing, is not an image.
pub fn read_image() -> Option<rhema_core::Image> {
    let grabbed = arboard::Clipboard::new()
        .and_then(|mut clipboard| clipboard.get_image())
        .ok()?;
    let (width, height): (u32, u32) = match (grabbed.width.try_into(), grabbed.height.try_into()) {
        (Ok(w), Ok(h)) => (w, h),
        // A frame larger than the pixel types agree on encodes as
        // nothing.
        _ => return None,
    };
    let rgba = image::RgbaImage::from_raw(width, height, grabbed.bytes.into_owned())?;
    let mut png = Vec::new();
    rgba.write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
        .ok()?;
    Some(rhema_core::Image::new(
        rhema_core::ImageMediaType::Png,
        base64::engine::general_purpose::STANDARD.encode(png),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The clipboard itself has no test double: what is testable here is
    /// the pure tail of the read — RGBA pixels in, base64 PNG out, the
    /// exact form a message block carries.
    #[test]
    fn a_copy_encodes_as_an_osc52_sequence() {
        assert_eq!(osc52("hello"), "\x1b]52;c;aGVsbG8=\x1b\\");
    }

    #[test]
    fn rgba_pixels_encode_as_bare_base64_png() {
        let image = encode_png(1, 1, vec![255, 0, 0, 255]).unwrap();
        assert_eq!(image.media_type, rhema_core::ImageMediaType::Png);
        assert!(
            !image.data.contains("data:"),
            "bare base64, no data: prefix: {}",
            image.data
        );
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(&image.data)
            .unwrap();
        // The encoding is real PNG, by the same magic-byte rule a
        // block's media_type answers to.
        assert_eq!(
            rhema_core::ImageMediaType::sniff(&decoded),
            Some(rhema_core::ImageMediaType::Png)
        );
    }

    /// Pixels that do not fill their frame encode as nothing — the
    /// length must match `width * height * 4`, or the shape is a lie.
    #[test]
    fn a_short_pixel_buffer_encodes_as_none() {
        assert!(encode_png(2, 2, vec![0, 0, 0, 0]).is_none());
    }

    /// The pure tail of [`read_image`], callable without a clipboard.
    fn encode_png(width: u32, height: u32, rgba: Vec<u8>) -> Option<rhema_core::Image> {
        let rgba = image::RgbaImage::from_raw(width, height, rgba)?;
        let mut png = Vec::new();
        rgba.write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .ok()?;
        Some(rhema_core::Image::new(
            rhema_core::ImageMediaType::Png,
            base64::engine::general_purpose::STANDARD.encode(png),
        ))
    }
}
