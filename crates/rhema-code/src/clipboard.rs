//! The system clipboard, read side only: ctrl-v as a paste key where the
//! terminal does not bind it itself. A fresh handle per read — the
//! connection is not held open between presses, and a session without a
//! clipboard (a pipe, an ssh hop) simply reads as none.

/// The clipboard's text, when one is reachable.
pub fn read() -> Option<String> {
    arboard::Clipboard::new()
        .and_then(|mut clipboard| clipboard.get_text())
        .ok()
        .filter(|text| !text.is_empty())
}
