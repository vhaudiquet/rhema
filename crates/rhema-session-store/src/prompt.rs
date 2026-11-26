//! The opening prompt: the one line a picker row previews.

use rhema_core::{Content, Role, Session};

/// How much of the opening prompt the listing row carries. Wide enough
/// for the pickers' panes; a host draws what its own width leaves and
/// ellipsizes the rest.
pub const OPENING_PROMPT_WIDTH: usize = 200;

/// The session's first non-empty user text, cut to
/// [`OPENING_PROMPT_WIDTH`], the picker's preview. `None` when the history
/// carries no user text at all (an empty conversation, or images alone).
pub fn opening_prompt(session: &Session) -> Option<String> {
    session
        .history
        .iter()
        .filter(|message| message.role == Role::User)
        .find_map(|message| {
            message.content.iter().find_map(|content| match content {
                Content::Text(text) if !text.trim().is_empty() => Some(text),
                _ => None,
            })
        })
        .map(|text| truncate_chars(text, OPENING_PROMPT_WIDTH))
}

/// Cuts `text` to `max` and ends the cut string with `…` so a row never
/// reads as complete when it is not.
///
/// The budget is a byte prefix (`max` bytes, ellipsis included); the cut
/// backs up to a character boundary, so multibyte text keeps its
/// characters whole and may end shorter than `max`. ASCII text cuts at
/// exactly `max` characters. One exception: a budget that lands inside
/// the first character shows that character whole instead of shrinking
/// the row to the lone ellipsis, so a cut row always names something of
/// its prompt. This is the width the TUI's picker and argument rows
/// have always used.
pub fn truncate_chars(text: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    if text.chars().count() <= max {
        return text.to_owned();
    }
    let mut end = max - 1;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    if end == 0 {
        end = text.chars().next().map_or(0, char::len_utf8);
    }
    format!("{}\u{2026}", &text[..end])
}

#[cfg(test)]
mod tests {
    use super::*;
    use rhema_core::Message;

    fn session_with(history: Vec<Message>) -> Session {
        Session {
            history,
            ..Session::default()
        }
    }

    #[test]
    fn the_first_non_empty_user_text_wins() {
        let session = session_with(vec![
            Message::text(Role::Assistant, "hello"),
            Message::text(Role::User, "   "),
            Message::text(Role::User, "first real prompt"),
            Message::text(Role::User, "second prompt"),
        ]);
        assert_eq!(
            opening_prompt(&session).as_deref(),
            Some("first real prompt")
        );
    }

    #[test]
    fn an_image_only_first_message_does_not_hide_later_text() {
        let session = session_with(vec![
            Message {
                annotations: Default::default(),
                role: Role::User,
                content: vec![Content::Image(rhema_core::Image::new(
                    rhema_core::ImageMediaType::Png,
                    "AAAA",
                ))],
            },
            Message::text(Role::User, "described later"),
        ]);
        assert_eq!(opening_prompt(&session).as_deref(), Some("described later"));
    }

    #[test]
    fn no_user_text_reads_as_none() {
        assert_eq!(opening_prompt(&Session::default()), None);
        let session = session_with(vec![Message::text(Role::Assistant, "only me")]);
        assert_eq!(opening_prompt(&session), None);
    }

    #[test]
    fn the_prompt_is_cut_to_the_row_width() {
        let long = "x".repeat(OPENING_PROMPT_WIDTH + 10);
        let session = session_with(vec![Message::text(Role::User, long)]);
        let prompt = opening_prompt(&session).unwrap();
        assert_eq!(prompt.chars().count(), OPENING_PROMPT_WIDTH);
        assert!(prompt.ends_with('\u{2026}'));
    }

    #[test]
    fn truncation_keeps_short_text_whole_and_never_splits_a_character() {
        assert_eq!(truncate_chars("short", 48), "short");
        assert_eq!(truncate_chars("", 0), "");
        // A multibyte character is never split at the cut; the cut
        // backs up to the last whole character before the byte budget.
        assert_eq!(truncate_chars("ééééé", 3), "é\u{2026}");
        assert_eq!(truncate_chars("héllo wörld", 4), "hé\u{2026}");
        // A budget landing inside the first character shows it whole
        // rather than degenerating to the lone ellipsis: the row
        // always names something of its prompt.
        assert_eq!(truncate_chars("🎉 a prompt", 2), "🎉\u{2026}");
        assert_eq!(truncate_chars("f", 1), "f");
        assert_eq!(truncate_chars("first prompt", 1), "f\u{2026}");
    }
}
