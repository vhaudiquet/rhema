//! The full-screen pickers: the model picker's screen host and the
//! resume picker over saved sessions. The model picker's state and keys
//! live in [`super::model_picker`], shared with the wizard and the
//! settings' Providers page.

use super::*;

/// One row of the resume picker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResumeRow {
    /// The saved session's file; loading it is the app's job.
    pub path: std::path::PathBuf,
    /// The row's age, computed at construction ("3m ago").
    pub age: String,
    /// The session's opening prompt, cut for the row; `None` when the
    /// file could not be read for a preview.
    pub snippet: Option<String>,
    /// The saved file's byte length, shown on the metadata line.
    pub bytes: u64,
}

/// What one resume-picker keypress means.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResumeAction {
    /// Nothing to do.
    None,
    /// Load the saved session at this path.
    Pick(std::path::PathBuf),
    /// Return to the chat.
    BackToChat,
}

/// The resume picker: one highlighted row, arrows to move, enter to
/// load, esc back to the chat. Sessions for one workspace only, newest
/// first, the listing decided that before the screen existed.
#[derive(Debug)]
pub struct ResumeScreen {
    /// The rows, newest first.
    pub entries: Vec<ResumeRow>,
    /// Index of the highlighted row.
    pub selected: usize,
    /// True while the listing runs off-thread: the screen paints
    /// before the session files are read, and an Enter waits.
    pub loading: bool,
}

impl ResumeScreen {
    /// A picker over the saved sessions; `now` (millis) fixes the age
    /// labels.
    pub fn new(entries: Vec<crate::sessions::SavedSession>, now: u64) -> Self {
        let entries = entries
            .into_iter()
            .map(|saved| ResumeRow {
                path: saved.path,
                age: crate::sessions::ago(saved.saved_at, now),
                snippet: saved.snippet,
                bytes: saved.bytes,
            })
            .collect();
        Self {
            entries,
            selected: 0,
            loading: false,
        }
    }

    /// The screen before its listing lands: no rows, no pick.
    pub fn loading() -> Self {
        Self {
            entries: Vec::new(),
            selected: 0,
            loading: true,
        }
    }

    /// Handles one keypress.
    pub fn handle_key(&mut self, key: KeyEvent) -> ResumeAction {
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            return ResumeAction::None;
        }
        match key.code {
            KeyCode::Down => {
                if !self.entries.is_empty() {
                    self.selected = (self.selected + 1).min(self.entries.len() - 1);
                }
                ResumeAction::None
            }
            KeyCode::Up => {
                self.selected = self.selected.saturating_sub(1);
                ResumeAction::None
            }
            KeyCode::Enter => self.pick(),
            KeyCode::Esc => ResumeAction::BackToChat,
            _ => ResumeAction::None,
        }
    }

    /// Enter: load the highlighted session. A listing still running
    /// has nothing to load yet.
    fn pick(&mut self) -> ResumeAction {
        if self.loading {
            return ResumeAction::None;
        }
        match self.entries.get(self.selected) {
            Some(row) => ResumeAction::Pick(row.path.clone()),
            None => ResumeAction::None,
        }
    }
}

// --- Model picker screen ---

/// The full-screen model picker: the shared [`ModelPicker`] over the
/// whole terminal, `/switch`'s screen. The pick rides the session only;
/// the persisted pick lives on the settings' Models page, where it is a
/// setting like the rest.
#[derive(Debug)]
pub struct PickerScreen {
    /// The widget itself: rows, filter, selection, status.
    pub picker: ModelPicker,
}

impl PickerScreen {
    /// A `/switch` screen over `models`.
    pub fn new(models: Vec<CatalogModel>) -> Self {
        Self {
            picker: ModelPicker::new(models),
        }
    }

    /// Handles one keypress through the shared widget.
    pub fn handle_key(&mut self, key: KeyEvent) -> ModelPickerAction {
        self.picker.handle_key(key)
    }
}

impl std::ops::Deref for PickerScreen {
    type Target = ModelPicker;

    fn deref(&self) -> &ModelPicker {
        &self.picker
    }
}

impl std::ops::DerefMut for PickerScreen {
    fn deref_mut(&mut self) -> &mut ModelPicker {
        &mut self.picker
    }
}
