//! Transcript mouse affordances: mapping screen cells onto the rows the
//! last frame drew, the plain text between two positions, and the scroll
//! rail's pointer mapping.
//!
//! The terminal owns the pointer while the TUI runs (mouse capture is on
//! for the wheel and the settings clicks), so selection cannot be the
//! terminal's own: the transcript keeps its drawn rows and maps the drag
//! onto them. The release copies the text, and Ctrl-C copies it again
//! while the highlight lives, the way a terminal's copy does.

use ratatui::layout::Rect;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// The transcript's last drawn frame: the inner rect the rows went into,
/// one plain-text string per drawn row, top row first, and the scroll
/// rail when the frame drew one. This is the coordinate space a mouse
/// selection or a rail drag maps onto; every frame replaces it, so both
/// always read what is on screen now.
#[derive(Debug, Default)]
pub(crate) struct TranscriptView {
    /// The rect the transcript rows were rendered into.
    pub(crate) area: Rect,
    /// The index of the first drawn row in the transcript's full row
    /// list. A selection pins it so streaming output cannot move the
    /// text under the pointer.
    pub(crate) top: usize,
    /// One plain-text string per drawn row, top row first.
    pub(crate) rows: Vec<String>,
    /// The scroll rail, when the rows ran past the viewport.
    pub(crate) rail: Option<ScrollRail>,
    /// The markdown links on the drawn rows: the cells a click opens.
    pub(crate) links: Vec<LinkHit>,
}

/// A markdown link as the last frame drew it: the cells its label
/// covers and the URL they stand for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LinkHit {
    /// The label's screen cells, one row high.
    pub(crate) area: Rect,
    /// The link's target.
    pub(crate) url: String,
}

/// The transcript's scroll rail as the last frame drew it: the track's
/// cells and the row counts a pointer maps onto a scroll offset. Every
/// frame replaces it, like [`TranscriptView`]; a transcript that fits
/// its viewport has none.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ScrollRail {
    /// The rail's cell rect: one column wide, the transcript's height.
    pub(crate) area: Rect,
    /// The transcript's total rendered rows.
    pub(crate) total: usize,
    /// The rows the viewport shows.
    pub(crate) visible: usize,
}

impl ScrollRail {
    /// The rows hidden above the viewport at its topmost position.
    fn max_top(&self) -> usize {
        self.total.saturating_sub(self.visible)
    }

    /// The thumb's height: the visible share of the content, at least
    /// one cell and never taller than the track.
    fn thumb_height(&self) -> usize {
        (self.visible * self.visible / self.total.max(1))
            .max(1)
            .min(self.visible)
    }

    /// The thumb's span at `top` (the first visible row): its row from
    /// the track's top and its height.
    pub(crate) fn thumb(&self, top: usize) -> (usize, usize) {
        let height = self.thumb_height();
        let span = self.visible.saturating_sub(height);
        let max_top = self.max_top();
        let row = (top.min(max_top) * span).checked_div(max_top).unwrap_or(0);
        (row, height)
    }

    /// The scroll offset a pointer at screen row `y` asks for: the
    /// thumb centers on the pointer, clamped to the track. The offset
    /// counts rows above the bottom, [`crate::screens::ChatState`]'s
    /// unit.
    pub(crate) fn scroll_at(&self, y: u16) -> usize {
        let height = self.thumb_height();
        let span = self.visible.saturating_sub(height);
        if span == 0 {
            return 0;
        }
        let row = y.saturating_sub(self.area.y) as usize;
        let offset = row.saturating_sub(height / 2).min(span);
        let max_top = self.max_top();
        max_top - offset * max_top / span
    }
}

impl TranscriptView {
    /// The URL drawn under a screen cell, when the last frame put a
    /// link label there.
    pub(crate) fn link_at(&self, x: u16, y: u16) -> Option<&str> {
        self.links
            .iter()
            .find(|link| link.area.contains(ratatui::layout::Position::new(x, y)))
            .map(|link| link.url.as_str())
    }

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
/// drag started on and the cell it reached, plus the transcript's first
/// visible row when the drag began.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Selection {
    /// Where the left button went down.
    pub(crate) anchor: (u16, u16),
    /// Where the drag last reached.
    pub(crate) cursor: (u16, u16),
    /// The first row the viewport drew when the selection started. The
    /// render keeps this row pinned while the selection lives, so output
    /// appended below does not slide the selected text out from under
    /// the highlight.
    pub(crate) viewport_top: usize,
}

impl Selection {
    /// A selection starting at one cell, over the viewport whose first
    /// drawn row is `viewport_top`.
    pub(crate) fn new(anchor: (u16, u16), viewport_top: usize) -> Self {
        Self {
            anchor,
            cursor: anchor,
            viewport_top,
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
            top: 0,
            rows: rows.iter().map(|row| (*row).to_owned()).collect(),
            rail: None,
            links: Vec::new(),
        }
    }

    #[test]
    fn the_rail_thumb_covers_the_visible_share_and_follows_the_top() {
        let rail = ScrollRail {
            area: Rect::new(8, 0, 1, 10),
            total: 40,
            visible: 10,
        };
        // A quarter of the content shows: two cells of thumb.
        assert_eq!(rail.thumb(0), (0, 2));
        // Halfway through the content, the thumb sits halfway down.
        assert_eq!(rail.thumb(15), (4, 2));
        // At the topmost position (bottom of the content), it reaches
        // the track's end.
        assert_eq!(rail.thumb(30), (8, 2));
    }

    #[test]
    fn a_pointer_maps_onto_a_scroll_offset() {
        let rail = ScrollRail {
            area: Rect::new(8, 2, 1, 10),
            total: 40,
            visible: 10,
        };
        // The track's first row asks for the topmost content.
        assert_eq!(rail.scroll_at(2), 30);
        // The last row asks for the live tail.
        assert_eq!(rail.scroll_at(11), 0);
        // Above and below the track clamp to the same ends.
        assert_eq!(rail.scroll_at(0), 30);
        assert_eq!(rail.scroll_at(200), 0);
    }

    #[test]
    fn a_drag_takes_the_text_between_its_cells() {
        let view = view(&["hello", "world", "third"]);
        let mut selection = Selection::new((1, 0), 0);
        selection.extend((3, 1));
        assert_eq!(text(&view, selection), "ello\nwor");
    }

    #[test]
    fn a_drag_down_and_right_normalizes_its_endpoints() {
        let view = view(&["hello", "world"]);
        let mut selection = Selection::new((3, 1), 0);
        selection.extend((1, 0));
        assert_eq!(text(&view, selection), "ello\nwor");
    }

    #[test]
    fn the_end_column_is_excluded_and_trailing_air_is_trimmed() {
        let view = view(&["ab    ", "cd"]);
        let mut selection = Selection::new((0, 0), 0);
        selection.extend((20, 1));
        assert_eq!(text(&view, selection), "ab\ncd");
    }

    #[test]
    fn a_selection_starting_on_a_drawn_row_and_ending_below_copies_to_the_last_row() {
        let view = view(&["one", "two"]);
        let mut selection = Selection::new((0, 0), 0);
        selection.extend((1, 9));
        assert_eq!(text(&view, selection), "one\ntwo");
    }

    #[test]
    fn a_selection_starting_below_the_rows_is_empty() {
        let view = view(&["one", "two"]);
        let selection = Selection::new((0, 9), 0);
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
