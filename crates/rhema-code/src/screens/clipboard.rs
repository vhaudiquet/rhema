//! The system clipboard, read side only: ctrl-v as a paste key where the
//! terminal does not bind it itself. A fresh handle per read — the
//! connection is not held open between presses, and a session without a
//! clipboard (a pipe, an ssh hop) simply reads as none.

use base64::Engine;

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
