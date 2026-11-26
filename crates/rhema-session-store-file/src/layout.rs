//! The workspace directory name.

use std::path::{Path, PathBuf};

/// The workspace directory name: the absolute workspace path with every
/// byte outside `[-._0-9A-Za-z]` percent-encoded.
///
/// The encoding is frozen. A directory written by an older build must
/// keep resolving, so the allowed set and the uppercase hex digits are
/// part of the store's contract, not an implementation detail.
pub fn encode_workspace(workspace: &Path) -> String {
    let mut out = String::new();
    for byte in workspace.to_string_lossy().bytes() {
        match byte {
            b'-' | b'.' | b'_' | b'0'..=b'9' | b'A'..=b'Z' | b'a'..=b'z' => out.push(byte as char),
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// The workspace path [`encode_workspace`] encoded, or `None` when the
/// name is not a whole percent-encoded UTF-8 path.
///
/// The decoding mirrors the frozen encoding, so a directory written by
/// any build resolves back to the path that made it. Anything but
/// `%XX` passes through, and a stray `%` fails rather than guessing.
pub fn decode_workspace(name: &str) -> Option<PathBuf> {
    let bytes = name.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while at < bytes.len() {
        if bytes[at] == b'%' {
            let high = hex_digit(*bytes.get(at + 1)?)?;
            let low = hex_digit(*bytes.get(at + 2)?)?;
            out.push(high << 4 | low);
            at += 3;
        } else {
            out.push(bytes[at]);
            at += 1;
        }
    }
    String::from_utf8(out).ok().map(PathBuf::from)
}

/// One hexadecimal digit, either case, as its value.
fn hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn distinct_paths_never_share_a_name() {
        assert_ne!(
            encode_workspace(Path::new("/a/b")),
            encode_workspace(Path::new("/a_b"))
        );
        assert_eq!(encode_workspace(Path::new("/a/b")), "%2Fa%2Fb");
    }

    #[test]
    fn decoding_reverses_the_encoding() {
        for path in ["/a/b", "/tmp/my-proj.v2", "/home/u/proj"] {
            let path = Path::new(path);
            assert_eq!(
                decode_workspace(&encode_workspace(path)).as_deref(),
                Some(path)
            );
        }
        // A stray escape is not a path the encoding ever wrote.
        assert_eq!(decode_workspace("%2"), None);
        assert_eq!(decode_workspace("%zz"), None);
        // Percent-encoded bytes that are not UTF-8 do not name a path.
        assert_eq!(decode_workspace("%FF"), None);
    }

    #[test]
    fn the_alphabet_stays_readable() {
        assert_eq!(
            encode_workspace(Path::new("/tmp/my-proj.v2")),
            "%2Ftmp%2Fmy-proj.v2"
        );
    }
}
