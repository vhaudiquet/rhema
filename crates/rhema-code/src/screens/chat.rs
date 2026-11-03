//! The chat screen state: the input, its chips, scrolling, and the actions the input routes.

use super::*;

/// What one chat keypress means.
#[derive(Debug, Clone, PartialEq)]
pub enum ChatAction {
    /// Nothing to do.
    None,
    /// Enter on the (possibly empty) input; the app drops empty and
    /// suppresses submits while a turn runs. The text is the session
    /// form — paste payloads embedded at their anchors — which the app
    /// pushes to the transcript as-is and expands only where plain
    /// text is wanted (commands, the provider). The images ride beside
    /// the text, in their anchors' order.
    Submit {
        /// The session-form text.
        text: String,
        /// Images submitted beside the text.
        images: Vec<rhema_core::Image>,
    },
}

/// One chip anchored in the input: the payload a [`ChatState::MARK`]
/// carries. Text pastes and clipboard images anchor the same way and
/// render the same — one atomic cell — and differ only in what submit
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
    /// appear in `input` — a 1:1 pairing the edit paths keep true. Chips
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

impl ChatState {
    /// Rows per PageUp/PageDown press — one mouse-wheel tick, so both
    /// controls step the history identically.
    pub const SCROLL_STEP: usize = 3;
    /// A paste at or under this many lines joins the input inline;
    /// longer ones become attachments.
    pub const PASTE_INLINE_LINES: usize = 3;
    /// The object-replacement character: one attachment anchor in
    /// `input`. It is one cursor step, and backspace or delete on it
    /// removes the chip and its payload together. Only [`Self::paste`]
    /// mints it, always paired with a payload — text arriving with
    /// marks of its own loses them, so a chip glyph copied back out of
    /// the box can never anchor empty.
    pub const MARK: char = rhema_core::paste::MARK;

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
    /// it becomes a chip anchored at the cursor — one cell in the box at
    /// the paste's place, full content on submit.
    pub fn paste(&mut self, text: &str) {
        let normalized = Self::normalize(text);
        if normalized.lines().count() > Self::PASTE_INLINE_LINES {
            self.chips.push(Chip::Paste(normalized));
            self.insert_char(Self::MARK);
        } else {
            self.insert_str(&normalized);
        }
    }

    /// A clipboard image landed: always a chip anchored at the cursor,
    /// never inline — an image has no inline form, and a prompt carries
    /// it as one cell beside the text, full payload on submit.
    pub fn paste_image(&mut self, image: rhema_core::Image) {
        self.chips.push(Chip::Image(image));
        self.insert_char(Self::MARK);
    }

    /// Terminal text made insertable: carriage returns normalize to
    /// newlines, and anchor marks are dropped — a mark is only ever
    /// minted by [`Self::paste`], paired with a payload.
    fn normalize(text: &str) -> String {
        rhema_core::paste::normalize(text)
    }

    /// Inserts one character at the cursor.
    fn insert_char(&mut self, c: char) {
        self.input.insert(self.byte_offset(), c);
        self.cursor += 1;
        self.palette_index = 0;
    }

    /// Removes `input[start..end]` (byte offsets) and, with it, the
    /// chip of every anchor in the range, paste or image — the 1:1
    /// mark-to-chip pairing survives any deletion.
    fn delete_range(&mut self, start: usize, end: usize) {
        let dropped = self.input[start..end].matches(Self::MARK).count();
        if dropped > 0 {
            let first = self.input[..start].matches(Self::MARK).count();
            self.chips.drain(first..first + dropped);
        }
        self.input.replace_range(start..end, "");
    }

    /// What a submit sends: the typed input with every paste embedded at
    /// its anchor, and the images beside it in anchor order — the
    /// session's record of the prompt. The transcript renders each embed
    /// as its chip, and the model sees the plain expansion
    /// (`rhema_core::paste::expand`, applied at the provider seam), never
    /// the encoding. Image anchors drop out of the text: an image
    /// travels as a block beside it, and an anchor left in would desync
    /// `embed`'s positional pairing of marks to payloads.
    fn submission(&self) -> (String, Vec<rhema_core::Image>) {
        let mut marked = String::with_capacity(self.input.len());
        let mut pastes = Vec::new();
        let mut images = Vec::new();
        for (index, piece) in self.input.split(Self::MARK).enumerate() {
            if index > 0 {
                match self.chips.get(index - 1) {
                    Some(Chip::Paste(payload)) => {
                        pastes.push(payload.clone());
                        marked.push(Self::MARK);
                    }
                    Some(Chip::Image(image)) => images.push(image.clone()),
                    None => {}
                }
            }
            marked.push_str(piece);
        }
        (rhema_core::paste::embed(&marked, &pastes), images)
    }

    /// Inserts `text` at the cursor, newlines and all — the bracketed
    /// paste path. Carriage returns normalize to newlines, since a
    /// terminal paste may carry either.
    pub fn insert_str(&mut self, text: &str) {
        let normalized = Self::normalize(text);
        let count = normalized.chars().count();
        self.input.insert_str(self.byte_offset(), &normalized);
        self.cursor += count;
        self.palette_index = 0;
    }

    /// The cursor's logical line index, 0-based — the input window's
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

    /// Byte offset of the cursor inside `input` (a char boundary).
    fn byte_offset(&self) -> usize {
        self.input
            .char_indices()
            .nth(self.cursor)
            .map(|(index, _)| index)
            .unwrap_or(self.input.len())
    }

    /// Handles one keypress. Ctrl combos are the app's (quit, model
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
        // not bind it themselves: the clipboard is read directly, the
        // same content a bracketed paste would have carried. A clipboard
        // holds one thing or the other — an image is tried first and the
        // text falls through.
        if key.code == KeyCode::Char('v')
            && key.modifiers.contains(KeyModifiers::CONTROL)
            && !key.modifiers.contains(KeyModifiers::ALT)
        {
            if let Some(image) = crate::clipboard::read_image() {
                self.paste_image(image);
            } else if let Some(text) = crate::clipboard::read() {
                self.paste(&text);
            }
            return ChatAction::None;
        }
        // AltGr lands as CONTROL|ALT on most terminals outside the kitty
        // protocol: a printable char carrying both is composed text, not
        // a combo — on French AZERTY, `@` is AltGr+0.
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
            // The palette owns the vertical arrows while it has rows —
            // commands, models under `/model`, profiles under
            // `/profile`, or the path entries under the cursor's token.
            KeyCode::Up | KeyCode::Down
                if !palette_rows(&self.input, self.cursor, models, profiles, sandboxes, icons)
                    .is_empty() =>
            {
                let count =
                    palette_rows(&self.input, self.cursor, models, profiles, sandboxes, icons)
                        .len();
                if key.code == KeyCode::Down {
                    self.palette_index = (self.palette_index + 1).min(count - 1);
                } else {
                    self.palette_index = self.palette_index.saturating_sub(1);
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
                let rows =
                    palette_rows(&self.input, self.cursor, models, profiles, sandboxes, icons);
                if let Some(row) = rows.get(self.palette_index.min(rows.len().saturating_sub(1))) {
                    match row {
                        PaletteRow::Command(spec) => {
                            // The input is replaced wholesale: any
                            // anchors in it go, and their payloads go
                            // with them.
                            self.chips.clear();
                            self.input = spec.name.to_owned();
                            self.cursor = self.input.chars().count();
                        }
                        PaletteRow::Model { id, .. } => {
                            self.chips.clear();
                            self.input = format!("/model {id}");
                            self.cursor = self.input.chars().count();
                        }
                        PaletteRow::Profile { name, .. } => {
                            self.chips.clear();
                            self.input = format!("/profile {name}");
                            self.cursor = self.input.chars().count();
                        }
                        PaletteRow::Choice { command, value, .. } => {
                            self.chips.clear();
                            self.input = format!("{command} {value}");
                            self.cursor = self.input.chars().count();
                        }
                        PaletteRow::Sandbox { value, .. } => {
                            self.chips.clear();
                            self.input = format!("/sandbox {value}");
                            self.cursor = self.input.chars().count();
                        }
                        PaletteRow::Path {
                            token_start,
                            token_end,
                            completion,
                            ..
                        } => {
                            // A local splice: anchors outside the token
                            // keep their payloads, and delete_range
                            // drops any inside it.
                            self.cursor = (*token_end).min(self.input.chars().count());
                            let end = self.byte_offset();
                            self.cursor = *token_start;
                            let start = self.byte_offset();
                            self.delete_range(start, end);
                            self.insert_str(completion);
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
                // A new prompt goes to the live tail; stop pinning
                // history. The input is NOT cleared: the app clears when
                // it accepts the submit, so a declined one keeps it.
                self.scroll = 0;
                let (text, images) = self.submission();
                ChatAction::Submit { text, images }
            }
            _ => ChatAction::None,
        }
    }
}
