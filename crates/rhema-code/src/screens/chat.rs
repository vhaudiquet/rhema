//! The chat screen state: the input, its chips, scrolling, and the actions the input routes.

use super::*;

/// What one chat keypress means.
#[derive(Debug, Clone, PartialEq)]
pub enum ChatAction {
    /// Nothing to do.
    None,
    /// Enter on the (possibly empty) input; the app drops empty and
    /// suppresses submits while a turn runs. The text is plain, paste
    /// payloads in place, and the spans name the ranges the transcript
    /// draws as chips. The images ride beside the text, in their
    /// anchors' order.
    Submit {
        /// The prompt's text, payloads in place.
        text: String,
        /// Images submitted beside the text.
        images: Vec<rhema_core::Image>,
        /// The paste spans the message records, in prompt order.
        spans: Vec<crate::paste::PasteSpan>,
    },
    /// Something to say, not to do: a notice the app draws in the
    /// transcript, the paste key's only way out for a fact about the
    /// environment the input box cannot act on.
    Notice {
        /// The notice's text.
        text: String,
    },
    /// The paste key: read the clipboard and route what it held
    /// through `paste_grab`. The connection round trip and any image's
    /// PNG re-encode are blocking work, so the app runs the read off
    /// the key handler and lands the grab back here as an event.
    Paste,
}

/// One chip anchored in the input: the payload a [`ChatState::MARK`]
/// carries. Text pastes and clipboard images anchor the same way and
/// render the same, one atomic cell, and differ only in what submit
/// does with them.
#[derive(Debug, Clone, PartialEq)]
pub enum Chip {
    /// A multi-line text paste's payload.
    Paste(String),
    /// A clipboard image, carried as bare base64.
    Image(rhema_core::Image),
}

/// The chat screen's input-box state: text, cursor (character index), and
/// the transcript scroll offset (rows kept above the bottom).
///
/// The transcript word-wraps, so scroll counts *rendered rows*, not
/// logical lines; render clamps it to the content height.
#[derive(Debug, Default)]
pub struct ChatState {
    /// The input being typed. An [`Self::MARK`] character anchors an
    /// attachment chip at the position its paste landed.
    pub input: String,
    /// The anchors' payloads, in the order their [`Self::MARK`] anchors
    /// appear in `input`, a 1:1 pairing the edit paths keep true. Chips
    /// render at their anchors; submit embeds the paste payloads in
    /// place and hands the images over beside the text.
    pub chips: Vec<Chip>,
    /// Cursor position as a character index into `input`.
    pub cursor: usize,
    /// Transcript scroll: rendered rows scrolled up beyond the bottom
    /// (0 follows the latest line).
    pub scroll: usize,
    /// Highlighted row of the command palette while a slash input filters
    /// it; meaningless otherwise.
    pub palette_index: usize,
    /// The completion rows for the exact `(input, cursor)` they were
    /// built from. The popup rebuilds every frame otherwise, and the
    /// path rows behind it walk a directory each time.
    pub(crate) completion: Option<CompletionCache>,
}

/// The path-completion rows and the input they answer.
#[derive(Debug, Clone)]
pub(crate) struct CompletionCache {
    pub(crate) input: String,
    pub(crate) cursor: usize,
    pub(crate) rows: Vec<PaletteRow>,
}

/// The input a non-path palette row completes to: a command's name, or
/// a command with its argument. None for a path row, which splices over
/// the token it came from instead of replacing the line.
fn completion_input(row: &PaletteRow) -> Option<String> {
    match row {
        PaletteRow::Command(spec) => Some(spec.name.to_owned()),
        PaletteRow::Model {
            command,
            provider,
            id,
            ..
        } => Some(format!("{command} {provider}/{id}")),
        PaletteRow::Profile { name, .. } => Some(format!("/profile {name}")),
        PaletteRow::Choice { command, value, .. } => Some(format!("{command} {value}")),
        PaletteRow::Sandbox { value, .. } => Some(format!("/sandbox {value}")),
        PaletteRow::Path { .. } => None,
    }
}

impl ChatState {
    /// Rows per PageUp/PageDown press, one mouse-wheel tick, so both
    /// controls step the history identically.
    pub const SCROLL_STEP: usize = 3;
    /// A paste at or under this many lines joins the input inline;
    /// longer ones become attachments.
    pub const PASTE_INLINE_LINES: usize = 3;
    /// The object-replacement character: one attachment anchor in
    /// `input`. It is one cursor step, and backspace or delete on it
    /// removes the chip and its payload together. Only [`Self::paste`]
    /// mints it, always paired with a payload, text arriving with
    /// marks of its own loses them, so a chip glyph copied back out of
    /// the box can never anchor empty.
    pub const MARK: char = crate::paste::MARK;

    /// A fresh input box.
    pub fn new() -> Self {
        Self::default()
    }

    /// Scrolls the transcript by `rows` (positive looks back); used by the
    /// mouse wheel as well as the page keys.
    pub fn scroll_by(&mut self, rows: isize) {
        if rows >= 0 {
            self.scroll = self.scroll.saturating_add(rows as usize);
        } else {
            self.scroll = self.scroll.saturating_sub((-rows) as usize);
        }
    }

    /// Inserts a newline at the cursor: the input grows a line instead
    /// of submitting.
    pub fn insert_newline(&mut self) {
        self.insert_str("\n");
    }

    /// A paste landed. Short enough, it joins the input inline; longer,
    /// it becomes a chip anchored at the cursor, one cell in the box at
    /// the paste's place, full content on submit.
    pub fn paste(&mut self, text: &str) {
        let normalized = Self::normalize(text);
        if normalized.lines().count() > Self::PASTE_INLINE_LINES {
            let at = self.chip_index_at_cursor();
            self.chips.insert(at, Chip::Paste(normalized));
            self.insert_char(Self::MARK);
        } else {
            self.insert_str(&normalized);
        }
    }

    /// A clipboard image landed: always a chip anchored at the cursor,
    /// never inline, an image has no inline form, and a prompt carries
    /// it as one cell beside the text, full payload on submit.
    pub fn paste_image(&mut self, image: rhema_core::Image) {
        let at = self.chip_index_at_cursor();
        self.chips.insert(at, Chip::Image(image));
        self.insert_char(Self::MARK);
    }

    /// Routes one clipboard grab for the paste key: an image or a text
    /// paste lands exactly as [`Self::paste`] and [`Self::paste_image`]
    /// take it, a clipboard holding nothing is silence, and a
    /// clipboard that cannot be reached is a notice, over ssh an image
    /// cannot ride the hop, the file named in the prompt is the way.
    pub fn paste_grab(&mut self, grab: super::clipboard::Grab) -> ChatAction {
        match grab {
            super::clipboard::Grab::Image(image) => {
                self.paste_image(image);
                ChatAction::None
            }
            super::clipboard::Grab::Text(text) => {
                self.paste(&text);
                ChatAction::None
            }
            super::clipboard::Grab::Nothing => ChatAction::None,
            super::clipboard::Grab::Oversized(bytes) => ChatAction::Notice {
                text: format!(
                    "clipboard image too large: {bytes} bytes, the limit is {} MB; paste a smaller one",
                    rhema_bash::vision::MAX_IMAGE_BYTES / (1024 * 1024)
                ),
            },
            super::clipboard::Grab::Unreachable => ChatAction::Notice {
                text: super::clipboard::UNREACHABLE_NOTICE.to_owned(),
            },
        }
    }

    /// Terminal text made insertable: carriage returns normalize to
    /// newlines, and anchor marks are dropped, a mark is only ever
    /// minted by [`Self::paste`], paired with a payload.
    fn normalize(text: &str) -> String {
        crate::paste::normalize(text)
    }

    /// Inserts one character at the cursor.
    fn insert_char(&mut self, c: char) {
        self.input.insert(self.byte_offset(), c);
        self.cursor += 1;
        self.palette_index = 0;
    }

    /// The chip slot the cursor's next anchor takes: how many anchors
    /// sit before the cursor.
    ///
    /// `submission` pairs the k-th mark with `chips[k]`, so an anchor
    /// pasted mid-line has to take that slot. Appending the chip to the
    /// end instead left its mark earlier than its payload: two pastes
    /// before an existing one came out swapped.
    fn chip_index_at_cursor(&self) -> usize {
        let before = self.input[..self.byte_offset()].matches(Self::MARK).count();
        before.min(self.chips.len())
    }

    /// Removes `input[start..end]` (byte offsets) and, with it, the
    /// chip of every anchor in the range, paste or image, the 1:1
    /// mark-to-chip pairing survives any deletion.
    fn delete_range(&mut self, start: usize, end: usize) {
        let dropped = self.input[start..end].matches(Self::MARK).count();
        if dropped > 0 {
            let first = self.input[..start].matches(Self::MARK).count();
            self.chips.drain(first..first + dropped);
        }
        self.input.replace_range(start..end, "");
    }

    /// Splices a path completion over the token it came from, the
    /// splice half of a path row shared by Tab and Enter. Anchors
    /// outside the token keep their payloads; any inside it go with
    /// their range.
    fn splice_path(&mut self, token_start: usize, token_end: usize, completion: &str) {
        self.cursor = token_end.min(self.input.chars().count());
        let end = self.byte_offset();
        self.cursor = token_start;
        let start = self.byte_offset();
        self.delete_range(start, end);
        self.insert_str(completion);
    }

    /// What a submit sends: the typed input with every paste's payload
    /// in place of its anchor, the images beside it in anchor order, and
    /// the spans the message annotates so the transcript can draw each
    /// payload as its chip. Image anchors drop out of the text: an image
    /// travels as a block beside it.
    fn submission(&self) -> (String, Vec<rhema_core::Image>, Vec<crate::paste::PasteSpan>) {
        let mut text = String::with_capacity(self.input.len());
        let mut pastes = Vec::new();
        let mut images = Vec::new();
        for (index, piece) in self.input.split(Self::MARK).enumerate() {
            if index > 0 {
                match self.chips.get(index - 1) {
                    Some(Chip::Paste(payload)) => {
                        let start = text.len();
                        text.push_str(payload);
                        pastes.push(crate::paste::PasteSpan {
                            start,
                            end: text.len(),
                            lines: payload.lines().count(),
                        });
                    }
                    Some(Chip::Image(image)) => images.push(image.clone()),
                    None => {}
                }
            }
            text.push_str(piece);
        }
        (text, images, pastes)
    }

    /// Inserts `text` at the cursor, newlines and all, the bracketed
    /// paste path. Carriage returns normalize to newlines, since a
    /// terminal paste may carry either.
    pub fn insert_str(&mut self, text: &str) {
        let normalized = Self::normalize(text);
        let count = normalized.chars().count();
        self.input.insert_str(self.byte_offset(), &normalized);
        self.cursor += count;
        self.palette_index = 0;
    }

    /// The cursor's logical line index, 0-based, the input window's
    /// anchor. A test seam only: production observes the cursor through
    /// the rendering, never through these.
    #[cfg(test)]
    pub fn cursor_line_index(&self) -> usize {
        self.input
            .chars()
            .take(self.cursor)
            .filter(|c| *c == '\n')
            .count()
    }

    /// The cursor's column within its logical line.
    #[cfg(test)]
    pub fn cursor_column(&self) -> usize {
        let (start, _) = self.cursor_line();
        self.cursor - start
    }

    /// The cursor's logical line as `(start, char_len)`, newline
    /// characters excluded from the length.
    fn cursor_line(&self) -> (usize, usize) {
        let chars: Vec<char> = self.input.chars().collect();
        let pos = self.cursor.min(chars.len());
        let mut start = 0;
        for (index, c) in chars.iter().enumerate() {
            if index == pos {
                break;
            }
            if *c == '\n' {
                start = index + 1;
            }
        }
        let end = chars[pos..]
            .iter()
            .position(|c| *c == '\n')
            .map(|offset| pos + offset)
            .unwrap_or(chars.len());
        (start, end - start)
    }

    /// Moves the cursor one logical line up or down, keeping the column
    /// where the target line is long enough.
    fn move_vertical(&mut self, up: bool) {
        let chars: Vec<char> = self.input.chars().collect();
        let mut bounds: Vec<(usize, usize)> = Vec::new();
        let mut start = 0;
        for (index, c) in chars.iter().enumerate() {
            if *c == '\n' {
                bounds.push((start, index - start));
                start = index + 1;
            }
        }
        bounds.push((start, chars.len() - start));
        let line = bounds
            .iter()
            .position(|(s, l)| self.cursor >= *s && self.cursor <= s + l)
            .unwrap_or(bounds.len() - 1);
        let target = if up {
            line.checked_sub(1)
        } else {
            (line + 1 < bounds.len()).then_some(line + 1)
        };
        if let Some(t) = target {
            let (start, len) = bounds[t];
            self.cursor = start + (self.cursor - bounds[line].0).min(len);
        }
    }

    /// Byte offset of a character index inside `input` (a char boundary).
    fn byte_offset_at(&self, index: usize) -> usize {
        self.input
            .char_indices()
            .nth(index)
            .map(|(index, _)| index)
            .unwrap_or(self.input.len())
    }

    /// Byte offset of the cursor inside `input`.
    fn byte_offset(&self) -> usize {
        self.byte_offset_at(self.cursor)
    }

    /// The character index where the word left of the cursor starts:
    /// past its trailing whitespace, then past the run of non-whitespace
    /// before it.
    fn word_start(&self) -> usize {
        let chars: Vec<char> = self.input.chars().collect();
        let mut start = self.cursor.min(chars.len());
        while start > 0 && chars[start - 1].is_whitespace() {
            start -= 1;
        }
        while start > 0 && !chars[start - 1].is_whitespace() {
            start -= 1;
        }
        start
    }

    /// The character index where the word right of the cursor ends:
    /// past its leading whitespace, then past the run of non-whitespace
    /// after it. The cursor itself when it sits at the input's end.
    fn word_end(&self) -> usize {
        let chars: Vec<char> = self.input.chars().collect();
        let mut end = self.cursor.min(chars.len());
        while end < chars.len() && chars[end].is_whitespace() {
            end += 1;
        }
        while end < chars.len() && !chars[end].is_whitespace() {
            end += 1;
        }
        end
    }

    /// Moves the cursor to [`Self::word_start`]: Alt-Left, the readline
    /// `backward-word` shape.
    fn move_word_backward(&mut self) {
        self.cursor = self.word_start();
    }

    /// Moves the cursor to [`Self::word_end`]: Alt-Right, the readline
    /// `forward-word` shape.
    fn move_word_forward(&mut self) {
        self.cursor = self.word_end();
    }

    /// Deletes the word left of the cursor: its trailing whitespace,
    /// then the run of non-whitespace before it. Alt-Backspace is the
    /// readline `backward-kill-word` shape.
    fn delete_word_backward(&mut self) {
        let start = self.word_start();
        if start == self.cursor {
            return;
        }
        let end = self.byte_offset();
        self.delete_range(self.byte_offset_at(start), end);
        self.cursor = start;
    }

    /// Deletes the word right of the cursor, the forward counterpart of
    /// [`Self::delete_word_backward`]: leading whitespace, then the run
    /// of non-whitespace after it.
    fn delete_word_forward(&mut self) {
        let end = self.word_end();
        if end == self.cursor {
            return;
        }
        self.delete_range(self.byte_offset(), self.byte_offset_at(end));
    }

    /// Handles one keypress. Ctrl combos are the app's (copy, model
    /// picker); Esc is the app's (approval decline / quit confirm). The
    /// model catalog, profile registry, and sandbox instances feed the
    /// `/model`, `/profile`, and `/sandbox` palette rows; empty slices
    /// simply leave those rows empty, and the filesystem feeds path
    /// rows for the token under the cursor.
    pub fn handle_key(
        &mut self,
        key: KeyEvent,
        models: &[CatalogModel],
        profiles: &[PaletteProfile],
        sandboxes: &[PaletteSandbox],
        icons: &'static crate::icons::Icons,
    ) -> ChatAction {
        // Ctrl-J is readline's historical newline: it inserts even
        // though plain control combos are inert.
        if key.code == KeyCode::Char('j') && key.modifiers == KeyModifiers::CONTROL {
            self.insert_newline();
            return ChatAction::None;
        }
        // Ctrl-v (with or without shift) is paste in terminals that do
        // not bind it themselves: the clipboard read is the same
        // content a bracketed paste would have carried. The connection
        // round trip and any image's PNG re-encode are blocking work,
        // so the key only asks and the app runs the read off its
        // thread, landing the grab back through paste_grab: an
        // unreachable clipboard, an ssh hop or a plain TTY, is noticed
        // instead of silent.
        if key.code == KeyCode::Char('v')
            && key.modifiers.contains(KeyModifiers::CONTROL)
            && !key.modifiers.contains(KeyModifiers::ALT)
        {
            return ChatAction::Paste;
        }
        // AltGr lands as CONTROL|ALT on most terminals outside the kitty
        // protocol: a printable char carrying both is composed text, not
        // a combo, on French AZERTY, `@` is AltGr+0.
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && !(key.modifiers.contains(KeyModifiers::ALT) && matches!(key.code, KeyCode::Char(_)))
        {
            return ChatAction::None;
        }
        // Shift-enter reports as a distinct key only under the kitty
        // keyboard protocol; alt-enter and ctrl-j are the universal
        // newline keys.
        if key.code == KeyCode::Enter
            && (key.modifiers.contains(KeyModifiers::ALT)
                || key.modifiers.contains(KeyModifiers::SHIFT))
        {
            self.insert_newline();
            return ChatAction::None;
        }
        match key.code {
            KeyCode::Char(c) => {
                // A terminal without bracketed paste delivers a paste as
                // plain char keys: a mark arriving this way is a copied
                // chip glyph, and must not anchor without a payload.
                if c == Self::MARK {
                    return ChatAction::None;
                }
                // A terminal reporting every key but omitting the
                // shifted-alternate field leaves SHIFT on an unshifted
                // char; the case is ours to fix then.
                let c = if key.modifiers.contains(KeyModifiers::SHIFT) {
                    c.to_ascii_uppercase()
                } else {
                    c
                };
                self.input.insert(self.byte_offset(), c);
                self.cursor += 1;
                self.palette_index = 0;
                ChatAction::None
            }
            // Alt-Backspace and Alt-Delete erase by word, the readline
            // pair every shell and every editor binds.
            KeyCode::Backspace if key.modifiers.contains(KeyModifiers::ALT) => {
                self.delete_word_backward();
                self.palette_index = 0;
                ChatAction::None
            }
            KeyCode::Delete if key.modifiers.contains(KeyModifiers::ALT) => {
                self.delete_word_forward();
                self.palette_index = 0;
                ChatAction::None
            }
            KeyCode::Backspace => {
                if self.cursor > 0 {
                    let end = self.byte_offset();
                    self.cursor -= 1;
                    let start = self.byte_offset();
                    self.delete_range(start, end);
                    self.palette_index = 0;
                }
                ChatAction::None
            }
            KeyCode::Delete => {
                let start = self.byte_offset();
                if start < self.input.len() {
                    self.cursor += 1;
                    let end = self.byte_offset();
                    self.cursor -= 1;
                    self.delete_range(start, end);
                }
                ChatAction::None
            }
            // Alt-Left and Alt-Right move by word, the readline pair
            // every shell and every editor binds.
            KeyCode::Left if key.modifiers.contains(KeyModifiers::ALT) => {
                self.move_word_backward();
                ChatAction::None
            }
            KeyCode::Right if key.modifiers.contains(KeyModifiers::ALT) => {
                self.move_word_forward();
                ChatAction::None
            }
            KeyCode::Left => {
                self.cursor = self.cursor.saturating_sub(1);
                ChatAction::None
            }
            KeyCode::Right => {
                self.cursor = (self.cursor + 1).min(self.input.chars().count());
                ChatAction::None
            }
            KeyCode::Home => {
                let (start, _) = self.cursor_line();
                self.cursor = start;
                ChatAction::None
            }
            KeyCode::End => {
                let (start, len) = self.cursor_line();
                self.cursor = start + len;
                ChatAction::None
            }
            // The palette owns the vertical arrows while it has rows,
            // commands, models under `/model`, profiles under
            // `/profile`, or the path entries under the cursor's token.
            // One cached lookup per press: the directory walk behind the
            // path rows used to run three times per arrow key.
            KeyCode::Up | KeyCode::Down if !self.input.contains('\n') => {
                let count = palette_rows_cached(self, models, profiles, sandboxes, icons).len();
                if count > 0 {
                    if key.code == KeyCode::Down {
                        self.palette_index = (self.palette_index + 1).min(count - 1);
                    } else {
                        self.palette_index = self.palette_index.saturating_sub(1);
                    }
                }
                ChatAction::None
            }
            // A multi-line input hands the vertical arrows to the cursor.
            KeyCode::Up if self.input.contains('\n') => {
                self.move_vertical(true);
                ChatAction::None
            }
            KeyCode::Down if self.input.contains('\n') => {
                self.move_vertical(false);
                ChatAction::None
            }
            KeyCode::Tab => {
                let rows = palette_rows_cached(self, models, profiles, sandboxes, icons);
                if let Some(row) = rows.get(self.palette_index.min(rows.len().saturating_sub(1))) {
                    match row {
                        PaletteRow::Command(spec) => {
                            self.chips.clear();
                            // An argument command completes with the
                            // space that opens its argument rows; a
                            // no-argument name is already whole.
                            self.input = if spec.argument == crate::commands::Argument::None {
                                spec.name.to_owned()
                            } else {
                                format!("{} ", spec.name)
                            };
                            self.cursor = self.input.chars().count();
                            self.palette_index = 0;
                        }
                        PaletteRow::Path {
                            token_start,
                            token_end,
                            completion,
                            ..
                        } => {
                            self.splice_path(*token_start, *token_end, completion);
                        }
                        _ => {
                            if let Some(input) = completion_input(row) {
                                self.chips.clear();
                                self.input = input;
                                self.cursor = self.input.chars().count();
                                self.palette_index = 0;
                            }
                        }
                    }
                }
                ChatAction::None
            }
            KeyCode::PageUp => {
                self.scroll = self.scroll.saturating_add(Self::SCROLL_STEP);
                ChatAction::None
            }
            KeyCode::PageDown => {
                self.scroll = self.scroll.saturating_sub(Self::SCROLL_STEP);
                ChatAction::None
            }
            KeyCode::Enter => {
                // A highlighted row takes the key first. A command or
                // argument row runs the input it completes to; a path
                // row only splices itself into the prompt. With no row
                // the key is the ordinary submit.
                let rows = palette_rows_cached(self, models, profiles, sandboxes, icons);
                if let Some(row) = rows.get(self.palette_index.min(rows.len().saturating_sub(1))) {
                    if let Some(text) = completion_input(row) {
                        // A new prompt goes to the live tail; stop
                        // pinning history. The input is NOT cleared: the
                        // app clears when it accepts the submit, so a
                        // declined one keeps it.
                        self.scroll = 0;
                        return ChatAction::Submit {
                            text,
                            images: Vec::new(),
                            spans: Vec::new(),
                        };
                    }
                    if let PaletteRow::Path {
                        token_start,
                        token_end,
                        completion,
                        ..
                    } = row
                    {
                        self.splice_path(*token_start, *token_end, completion);
                        return ChatAction::None;
                    }
                }
                self.scroll = 0;
                let (text, images, spans) = self.submission();
                ChatAction::Submit {
                    text,
                    images,
                    spans,
                }
            }
            _ => ChatAction::None,
        }
    }
}
