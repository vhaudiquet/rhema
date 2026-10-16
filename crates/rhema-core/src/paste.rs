//! Embedded pastes in message text: the compact record a UI keeps so a
//! multi-line paste can travel as one chip.
//!
//! A paste is written as [`MARK`], its payload, [`MARK`] — one logical
//! unit inside an otherwise plain string. The form is the session's
//! record of the prompt: UIs render each embed as a chip (id, size),
//! and [`expand`] restores the plain text the model must see, so the
//! encoding never reaches a provider. Payloads are normalized on embed
//! (carriage returns to newlines, stray marks dropped) so the scan is
//! unambiguous.

/// The object-replacement character: opens and closes one embedded
/// paste. Two marks always bracket a payload; a lone mark is literal
/// text.
pub const MARK: char = '\u{FFFC}';

/// One scanned piece of embedded-form text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Part<'a> {
    /// Plain text between pastes; may contain newlines.
    Text(&'a str),
    /// One paste's payload, without its bracketing marks.
    Paste(&'a str),
}

/// Splits embedded-form text into its parts, left to right. A mark with
/// no partner is literal text; nothing panics on malformed input.
pub fn parts(text: &str) -> Vec<Part<'_>> {
    let mut parts = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find(MARK) {
        let after = &rest[start + MARK.len_utf8()..];
        match after.find(MARK) {
            Some(end) => {
                if start > 0 {
                    parts.push(Part::Text(&rest[..start]));
                }
                parts.push(Part::Paste(&after[..end]));
                rest = &after[end + MARK.len_utf8()..];
            }
            // Unpaired: literal text up to and including the mark.
            None => {
                parts.push(Part::Text(&rest[..start + MARK.len_utf8()]));
                rest = after;
            }
        }
    }
    if !rest.is_empty() {
        parts.push(Part::Text(rest));
    }
    parts
}

/// Expands embedded pastes back to plain text — the form a provider
/// sees. Malformed marks pass through untouched.
pub fn expand(text: &str) -> String {
    let mut full = String::with_capacity(text.len());
    for part in parts(text) {
        match part {
            Part::Text(text) => full.push_str(text),
            Part::Paste(payload) => full.push_str(payload),
        }
    }
    full
}

/// Normalizes text that travels inside an embed or beside marks:
/// carriage returns to newlines, stray marks dropped — the scan must
/// stay unambiguous.
pub fn normalize(text: &str) -> String {
    text.replace("\r\n", "\n")
        .replace('\r', "\n")
        .replace(MARK, "")
}

/// Writes `attachments` into `input` at the [`MARK`] anchors the UI
/// placed there: each mark becomes `MARK`, its payload, [`MARK`].
/// Payloads are normalized (see [`normalize`]); a mark with no payload
/// left expands to nothing.
pub fn embed(input: &str, attachments: &[String]) -> String {
    let mut embedded = String::with_capacity(input.len());
    let mut payloads = attachments.iter();
    for (index, piece) in input.split(MARK).enumerate() {
        if index > 0 {
            if let Some(payload) = payloads.next() {
                let payload = normalize(payload);
                embedded.push(MARK);
                embedded.push_str(&payload);
                embedded.push(MARK);
            }
        }
        embedded.push_str(piece);
    }
    embedded
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embed_expand_round_trips() {
        let long = "l1\nl2\nl3\nl4";
        assert_eq!(
            embed(&format!("look at {MARK} this"), &[long.to_owned()]),
            format!("look at {MARK}{long}{MARK} this")
        );
        let embedded = embed(&format!("a{MARK}b"), &[long.to_owned()]);
        assert_eq!(embedded, format!("a{MARK}{long}{MARK}b"));
        assert_eq!(expand(&embedded), format!("a{long}b"));
        // Marks at both ends of the input land the same way.
        let wrapped = embed(
            &format!("{MARK}{MARK}b"),
            &["p1".to_owned(), "p2".to_owned()],
        );
        assert_eq!(wrapped, format!("{MARK}p1{MARK}{MARK}p2{MARK}b"));
        assert_eq!(expand(&wrapped), "p1p2b");
    }

    #[test]
    fn parts_scans_marks_and_payloads() {
        let embedded = format!("x{MARK}p1{MARK}y{MARK}p2{MARK}");
        assert_eq!(
            parts(&embedded),
            vec![
                Part::Text("x"),
                Part::Paste("p1"),
                Part::Text("y"),
                Part::Paste("p2"),
            ]
        );
        // No marks: the whole text is one part. Empty input: no parts.
        assert_eq!(parts("plain"), vec![Part::Text("plain")]);
        assert!(parts("").is_empty());
    }

    #[test]
    fn an_unpaired_mark_stays_literal() {
        let stray = format!("a{MARK}b");
        assert_eq!(
            parts(&stray),
            vec![Part::Text(&format!("a{MARK}")), Part::Text("b")]
        );
        assert_eq!(expand(&stray), stray);
    }

    #[test]
    fn embed_normalizes_payloads() {
        let embedded = embed(
            &format!("x{MARK}"),
            &["p\r\nq".to_owned(), format!("r{MARK}s")],
        );
        assert_eq!(embedded, format!("x{MARK}p\nq{MARK}"));
        assert_eq!(expand(&embedded), "xp\nq");
    }
}
