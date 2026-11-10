//! Transcript mouse selection: mapping screen cells onto the rows the
//! last frame drew, and the plain text between two positions.
//!
//! The terminal owns the pointer while the TUI runs (mouse capture is on
//! for the wheel and the settings clicks), so selection cannot be the
//! terminal's own: the transcript keeps its drawn rows and maps the drag
//! onto them. The release copies the text, the way a terminal's copy
//! does.

use ratatui::layout::Rect;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// The transcript's last drawn frame: the inner rect the rows went into
/// and one plain-text string per drawn row, top row first. This is the
/// coordinate space a mouse selection maps onto; every frame replaces
/// it, so a selection always reads what is on screen now.
#[derive(Debug, Default)]
pub(crate) struct TranscriptView {
    /// The rect the transcript rows were rendered into.
    pub(crate) area: Rect,
    /// One plain-text string per drawn row, top row first.
    pub(crate) rows: Vec<String>,
}

impl TranscriptView {
    /// Whether the cell can start a selection: inside the rect and over
    /// a row that carries text (a short transcript leaves blank rows
    /// below it that select nothing).
    pub(crate) fn starts_at(&self, x: u16, y: u16) -> bool {
        self.area.width > 0
            && self.area.contains(ratatui::layout::Position::new(x, y))
            && self.row_index(y) < self.rows.len()
    }

    /// The row index a screen y maps onto, clamped onto the drawn rows.
    fn row_index(&self, y: u16) -> usize {
        y.saturating_sub(self.area.y) as usize
    }

    /// The display column a screen x maps onto, clamped into the rect.
    fn column(&self, x: u16) -> usize {
        (x.saturating_sub(self.area.x) as usize).min(self.area.width.saturating_sub(1) as usize)
    }

    /// The display width of one drawn row.
    fn row_width(&self, row: usize) -> usize {
        self.rows.get(row).map(|text| text.width()).unwrap_or(0)
    }
}

/// A selection's two endpoints, in absolute screen cells: the cell the
/// drag started on and the cell it reached.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Selection {
    /// Where the left button went down.
    pub(crate) anchor: (u16, u16),
    /// Where the drag last reached.
    pub(crate) cursor: (u16, u16),
}

impl Selection {
    /// A selection starting at one cell.
    pub(crate) fn new(anchor: (u16, u16)) -> Self {
        Self {
            anchor,
            cursor: anchor,
        }
    }

    /// The drag reached another cell.
    pub(crate) fn extend(&mut self, cursor: (u16, u16)) {
        self.cursor = cursor;
    }
}

/// The plain text between the selection's endpoints. A row slice runs
/// from the start column to the row's end, every middle row is whole,
/// and the last row stops at the end column; the end cell is excluded,
/// so a drag to a line's end takes every character. Trailing whitespace
/// on each row is trimmed, and the blank rows below the last line of a
/// downward drag are dropped.
pub(crate) fn text(view: &TranscriptView, selection: Selection) -> String {
    if view.rows.is_empty() {
        return String::new();
    }
    let (anchor, cursor) = if selection.anchor <= selection.cursor {
        (selection.anchor, selection.cursor)
    } else {
        (selection.cursor, selection.anchor)
    };
    let start_row = view.row_index(anchor.1);
    if start_row >= view.rows.len() {
        return String::new();
    }
    let raw_end_row = view.row_index(cursor.1);
    let end_row = raw_end_row.min(view.rows.len() - 1);
    // A drag below the last drawn row selects through that row's end,
    // the way a terminal's selection does, instead of stopping at the
    // column the pointer happened to sit on.
    let end_spills = raw_end_row > end_row;
    let start_col = view.column(anchor.0);
    let end_col = view.column(cursor.0);

    let mut rows = Vec::new();
    for row in start_row..=end_row {
        let line = &view.rows[row];
        let from = if row == start_row {
            char_start(line, start_col)
        } else {
            0
        };
        let to = if row == end_row && !end_spills {
            char_end(line, end_col)
        } else {
            line.chars().count()
        };
        let piece: String = line
            .chars()
            .skip(from)
            .take(to.saturating_sub(from))
            .collect();
        rows.push(piece.trim_end().to_owned());
    }
    while rows.last().is_some_and(String::is_empty) {
        rows.pop();
    }
    rows.join("\n")
}

/// Marks the selected cells reversed, the terminal's own selection look.
/// Only the text the rows carry is covered; the empty tail of a row and
/// the two-cell right margin stay plain.
pub(crate) fn highlight(
    frame: &mut ratatui::Frame<'_>,
    view: &TranscriptView,
    selection: Selection,
) {
    if view.rows.is_empty() {
        return;
    }
    let (anchor, cursor) = if selection.anchor <= selection.cursor {
        (selection.anchor, selection.cursor)
    } else {
        (selection.cursor, selection.anchor)
    };
    let start_row = view.row_index(anchor.1);
    if start_row >= view.rows.len() {
        return;
    }
    let raw_end_row = view.row_index(cursor.1);
    let end_row = raw_end_row.min(view.rows.len() - 1);
    let end_spills = raw_end_row > end_row;
    let start_col = view.column(anchor.0);
    let end_col = view.column(cursor.0);
    let area = view.area;
    let buffer = frame.buffer_mut();
    for row in start_row..=end_row {
        let row_width = view.row_width(row);
        let line = &view.rows[row];
        let from = if row == start_row {
            display_width(line, char_start(line, start_col))
        } else {
            0
        };
        let to = if row == end_row && !end_spills {
            display_width(line, char_end(line, end_col))
        } else {
            row_width
        }
        .min(area.width as usize);
        if from >= to {
            continue;
        }
        let y = area.y + row as u16;
        for x in area.x + from as u16..area.x + to as u16 {
            if let Some(cell) = buffer.cell_mut((x, y)) {
                cell.modifier.insert(ratatui::style::Modifier::REVERSED);
            }
        }
    }
}

/// The display column where the first `index` chars end.
fn display_width(text: &str, index: usize) -> usize {
    text.chars()
        .take(index)
        .map(|ch| UnicodeWidthChar::width(ch).unwrap_or(0))
        .sum()
}

/// The char index covering display column `col`: a column inside a wide
/// char lands on that char, so a selection starting on it keeps it.
fn char_start(text: &str, col: usize) -> usize {
    let mut width = 0;
    for (index, ch) in text.chars().enumerate() {
        let char_width = UnicodeWidthChar::width(ch).unwrap_or(0);
        if width + char_width > col {
            return index;
        }
        width += char_width;
    }
    text.chars().count()
}

/// The char index at the boundary column `col` belongs after: a column
/// inside a wide char ends just past it, so a selection ending on it
/// keeps the char whole.
fn char_end(text: &str, col: usize) -> usize {
    let mut width = 0;
    for (index, ch) in text.chars().enumerate() {
        if width >= col {
            return index;
        }
        width += UnicodeWidthChar::width(ch).unwrap_or(0);
    }
    text.chars().count()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view(rows: &[&str]) -> TranscriptView {
        TranscriptView {
            area: Rect::new(0, 0, 40, rows.len() as u16),
            rows: rows.iter().map(|row| (*row).to_owned()).collect(),
        }
    }

    #[test]
    fn a_drag_takes_the_text_between_its_cells() {
        let view = view(&["hello", "world", "third"]);
        let mut selection = Selection::new((1, 0));
        selection.extend((3, 1));
        assert_eq!(text(&view, selection), "ello\nwor");
    }

    #[test]
    fn a_drag_down_and_right_normalizes_its_endpoints() {
        let view = view(&["hello", "world"]);
        let mut selection = Selection::new((3, 1));
        selection.extend((1, 0));
        assert_eq!(text(&view, selection), "ello\nwor");
    }

    #[test]
    fn the_end_column_is_excluded_and_trailing_air_is_trimmed() {
        let view = view(&["ab    ", "cd"]);
        let mut selection = Selection::new((0, 0));
        selection.extend((20, 1));
        assert_eq!(text(&view, selection), "ab\ncd");
    }

    #[test]
    fn a_selection_starting_on_a_drawn_row_and_ending_below_copies_to_the_last_row() {
        let view = view(&["one", "two"]);
        let mut selection = Selection::new((0, 0));
        selection.extend((1, 9));
        assert_eq!(text(&view, selection), "one\ntwo");
    }

    #[test]
    fn a_selection_starting_below_the_rows_is_empty() {
        let view = view(&["one", "two"]);
        let selection = Selection::new((0, 9));
        assert_eq!(text(&view, selection), "");
    }

    #[test]
    fn a_wide_char_maps_by_its_cells() {
        // Starts snap onto the char covering the column; ends stop
        // after it, so selecting from either side keeps it whole.
        assert_eq!(char_start("a中b", 0), 0);
        assert_eq!(char_start("a中b", 1), 1);
        assert_eq!(char_start("a中b", 2), 1);
        assert_eq!(char_start("a中b", 3), 2);
        assert_eq!(char_end("a中b", 0), 0);
        assert_eq!(char_end("a中b", 1), 1);
        assert_eq!(char_end("a中b", 2), 2);
        assert_eq!(char_end("a中b", 3), 2);
        assert_eq!(char_end("a中b", 4), 3);
    }
}
