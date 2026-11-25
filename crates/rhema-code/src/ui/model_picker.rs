//! The model picker's render: the row list, the filter line, and the
//! providers that went missing. Every host hands it a rect and gets the
//! same widget, so `/switch`'s screen, the wizard's model step, and the
//! settings' Models page cannot drift apart. The width decides how much
//! of a row is drawn, not the caller.

use super::*;
use crate::screens::{ModelPicker, ModelPickerStatus, PickerRow, PickerScreen};
use ratatui::widgets::{List, ListItem, ListState};
use rhema_provider::ModelCard;

/// What a host adds to the shared widget: a title row above the list,
/// the key hints under it, and whether the picker takes keys.
pub(crate) struct PickerChrome<'a> {
    /// A heading drawn inside the area; `None` leaves the host's own
    /// chrome (a panel border, a page title) to name the picker.
    pub(crate) title: Option<&'a str>,
    /// The key hints the footer ends with. Empty when the host draws
    /// its own, and then the picker overrides nothing.
    pub(crate) hints: &'a str,
    /// Whether the picker is the thing the keys reach: the caret shows
    /// only then.
    pub(crate) focused: bool,
    /// What an empty filter line says while the picker does not take
    /// keys, so the host can name its own focus key. Empty when the
    /// picker always holds them.
    pub(crate) prompt: &'a str,
}

/// Draws the picker inside `area`, at whatever size the host gives it:
/// the title row when one is asked for and the area can spare it, the
/// list filling the rest, and the status line when height allows. A
/// one-row area is a list row; a zero-height or zero-width area draws
/// nothing. Returns the drawn rows with their index, so a host that
/// takes clicks knows where each one landed.
pub(crate) fn render_model_picker(
    frame: &mut Frame,
    area: Rect,
    picker: &ModelPicker,
    icons: &crate::icons::Icons,
    theme: &Theme,
    chrome: &PickerChrome<'_>,
) -> Vec<(Rect, usize)> {
    if area.width == 0 || area.height == 0 {
        return Vec::new();
    }
    let title_rows = u16::from(chrome.title.is_some() && area.height >= 2);
    let body_rows = area.height - title_rows;
    // The filter line keeps a row of air under the rows: the box that
    // edits them is not one of them. A pane too short for the air gives
    // it up rather than the rows.
    let gap_rows = u16::from(body_rows >= 4);
    let footer_rows = u16::from(body_rows - gap_rows >= 2);
    let [title_area, list_area, _gap, footer_area] = Layout::vertical([
        Constraint::Length(title_rows),
        Constraint::Min(0),
        Constraint::Length(gap_rows),
        Constraint::Length(footer_rows),
    ])
    .areas(area);
    if let Some(title) = chrome.title.filter(|_| title_rows > 0) {
        frame.render_widget(
            Paragraph::new(Line::from(Span::raw(title.to_owned()).bold())),
            title_area,
        );
    }
    let rows = render_rows(frame, list_area, picker, icons, theme);
    if footer_rows > 0 {
        let line = footer_line(picker, chrome, theme);
        frame.render_widget(
            Paragraph::new(truncate_line(line, area.width as usize)),
            footer_area,
        );
    }
    rows
}

/// The list, or the one line that says why there is none. Returns where
/// each drawn row landed.
fn render_rows(
    frame: &mut Frame,
    area: Rect,
    picker: &ModelPicker,
    icons: &crate::icons::Icons,
    theme: &Theme,
) -> Vec<(Rect, usize)> {
    let width = area.width as usize;
    // Every row gives the first two cells to the marker column, the
    // selected row's `❯ ` and a blank on the rest; the columns fill what
    // is left.
    let text_width = width.saturating_sub(2);
    let current = picker
        .current
        .as_ref()
        .map(|current| (current.provider.as_str(), current.id.as_str()));
    // The columns are measured over the whole catalog, not over the rows
    // a filter left: a table whose columns moved as you typed would be
    // unreadable, and "no match" would lose the titles that say what it
    // has no match for.
    let cards: Vec<(&str, &ModelCard)> = picker
        .catalog()
        .iter()
        .map(|entry| (entry.provider, &entry.model))
        .collect();
    let titled = !cards.is_empty();
    let columns = Columns::of(&cards, current, icons, text_width);
    // A titled header sits over the catalog rows, aligned with them: a
    // number with no name reads as noise. A row of air under it keeps
    // the titles from reading as the first row's. A pinned row alone has
    // no columns to name, so it gets no header.
    let header_rows = u16::from(titled && area.height >= 3);
    let [header_area, _air, list_area] = Layout::vertical([
        Constraint::Length(header_rows),
        Constraint::Length(u16::from(header_rows > 0)),
        Constraint::Min(0),
    ])
    .areas(area);
    if header_rows > 0 {
        frame.render_widget(
            Paragraph::new(columns.header(theme)).block(Block::default().padding(Padding::left(2))),
            header_area,
        );
    }
    let rows = picker.rows();
    // A fetch still out draws the rows it already has: they are real
    // rows a pick can land on, and the status line says more are on the
    // way. Only a list with nothing in it yet falls back to the message,
    // which lands in the data's own column, under the titles that name
    // the columns it found nothing for.
    if rows.is_empty() {
        let message = match &picker.status {
            // The spinner steps off the picker's own clock: a fetch that
            // takes a while says it is going somewhere.
            ModelPickerStatus::Loading => Line::from(vec![
                Span::raw(super::status::spinner_char(picker.loading_for()).to_string())
                    .fg(theme.accent_text),
                Span::raw(" loading models…").fg(theme.text3),
            ]),
            ModelPickerStatus::Error(message) => Line::from(vec![
                Span::raw(message.clone()).fg(theme.error),
                Span::raw("  press r to retry").fg(theme.text3),
            ]),
            ModelPickerStatus::Ready if picker.filter.is_empty() => {
                Line::from(Span::raw("no models").fg(theme.text3))
            }
            ModelPickerStatus::Ready => Line::from(Span::raw("no match").fg(theme.text3)),
        };
        let text_area = Rect {
            x: list_area.x.saturating_add(2),
            width: list_area.width.saturating_sub(2),
            height: list_area.height.min(1),
            ..list_area
        };
        frame.render_widget(
            Paragraph::new(truncate_line(message, text_area.width as usize)),
            text_area,
        );
        return Vec::new();
    }
    let items: Vec<ListItem> = rows
        .iter()
        .map(|row| ListItem::new(row_line(*row, current, &columns, icons, theme)))
        .collect();
    // The offset is set rather than left to the list: the host that
    // takes clicks needs the rows where they were actually drawn.
    let visible = list_area.height as usize;
    let offset = picker.selected.saturating_sub(visible.saturating_sub(1));
    let mut state = ListState::default();
    state.select(Some(picker.selected));
    *state.offset_mut() = offset;
    // Without an explicit highlight the selection is invisible: ListState
    // only offsets the scroll, the marker and style are opt-in.
    frame.render_stateful_widget(
        List::new(items)
            .highlight_symbol("❯ ")
            .highlight_style(Style::default().bg(theme.elevated))
            .style(Style::default().fg(theme.text)),
        list_area,
        &mut state,
    );
    let first = state.offset();
    (first..rows.len().min(first + visible))
        .map(|index| {
            let y = list_area.y.saturating_add((index - first) as u16);
            (
                Rect {
                    x: list_area.x,
                    y,
                    width: list_area.width,
                    height: 1,
                },
                index,
            )
        })
        .collect()
}

/// The space between two columns.
const COLUMN_GAP: usize = 2;

/// The narrowest the model column is drawn before a pane drops a column
/// instead of squeezing the names, which are what a pick is made on.
const MIN_MODEL: usize = 34;

/// The columns a row is laid out in, in cells, worked out once per draw
/// from what the list holds and the width the pane can give. The model
/// column takes what the fixed ones leave.
#[derive(Clone, Copy)]
struct Columns {
    model: usize,
    input: Option<usize>,
    output: Option<usize>,
    capabilities: Option<usize>,
    ctx: Option<usize>,
}

impl Columns {
    fn of(
        cards: &[(&str, &ModelCard)],
        current: Option<(&str, &str)>,
        icons: &crate::icons::Icons,
        width: usize,
    ) -> Self {
        let widest = |header: &str, cell: &dyn Fn(&ModelCard) -> String| {
            cards
                .iter()
                .map(|(_, model)| cell(model).width())
                .chain(std::iter::once(header.width()))
                .max()
                .unwrap_or(0)
        };
        // A column exists only when at least one row fills it: a header
        // over a stripe of blanks is noise, and it would cost the names
        // the width it holds.
        let any =
            |wanted: &dyn Fn(&ModelCard) -> bool| cards.iter().any(|(_, model)| wanted(model));
        let prices = any(&|model| model.pricing.is_some());
        let input = prices.then(|| widest("In", &|model| price_cell(model, true)));
        let output = prices.then(|| widest("Out", &|model| price_cell(model, false)));
        // The capability column holds glyphs, one per capability, so it
        // is sized for several even while vision is the only one the
        // catalogs publish.
        let capabilities = any(&|model| model.supports_vision == Some(true))
            .then(|| widest("Cap.", &|model| capability_cell(model, icons)));
        let ctx = any(&|model| model.input_token_limit.is_some())
            .then(|| widest("Ctx", &|model| format_tokens(model.input_token_limit)));
        // The model column hugs its longest name: a table with the
        // numbers at the pane's far edge reads as a scatter. `Model`
        // names it, so it is never narrower than that. A list with no
        // catalog row (the profile's pinned model alone) has no columns
        // to line up with, so nothing constrains it.
        let widest_model = if cards.is_empty() {
            width
        } else {
            cards
                .iter()
                .map(|(provider, model)| {
                    let in_use =
                        current.is_some_and(|(name, id)| name == *provider && id == model.id);
                    provider.width()
                        + 1
                        + model.id.width()
                        + ("  · in use".width() * usize::from(in_use))
                })
                .chain(std::iter::once("Model".width()))
                .max()
                .unwrap_or(0)
        };
        // The columns drop from the least to the most useful when the
        // pane cannot hold them all: a narrow pane keeps the names and
        // the context the longest. Out goes first, then In, then the
        // capabilities; the model column never gives way.
        let mut keep = [input, output, capabilities, ctx];
        let fixed = |keep: &[Option<usize>; 4]| -> usize {
            let widths: Vec<usize> = keep.iter().flatten().copied().collect();
            widths.iter().sum::<usize>() + COLUMN_GAP * widths.len()
        };
        let floor = MIN_MODEL.min(widest_model);
        for slot in [1usize, 0, 2, 3] {
            if width.saturating_sub(fixed(&keep)) >= floor {
                break;
            }
            keep[slot] = None;
        }
        Self {
            model: width.saturating_sub(fixed(&keep)).min(widest_model),
            input: keep[0],
            output: keep[1],
            capabilities: keep[2],
            ctx: keep[3],
        }
    }

    /// The cells the fixed columns take, gaps included.
    fn fixed(&self) -> usize {
        let widths: Vec<usize> = [self.input, self.output, self.capabilities, self.ctx]
            .into_iter()
            .flatten()
            .collect();
        widths.iter().sum::<usize>() + COLUMN_GAP * widths.len()
    }

    /// The header row: the columns' names over the columns' own cells,
    /// `Model` over the names it titles.
    fn header(&self, theme: &Theme) -> Line<'static> {
        let mut spans = vec![pad("Model", self.model)];
        for (width, name) in [
            (self.input, "In"),
            (self.output, "Out"),
            (self.capabilities, "Cap."),
            (self.ctx, "Ctx"),
        ] {
            if let Some(width) = width {
                spans.push(Span::raw(" ".repeat(COLUMN_GAP)));
                spans.push(pad(name, width));
            }
        }
        // Bold, a tone above the data it names: a title has to read as a
        // title in a pane whose rows are all the same weight.
        Line::from(
            spans
                .into_iter()
                .map(|span| Span::styled(span.content, span.style.fg(theme.text2).bold()))
                .collect::<Vec<_>>(),
        )
    }
}

/// A cell's text padded to its column, cut with an ellipsis when it
/// cannot fit: the columns stay in line whatever the data says.
fn pad(text: &str, width: usize) -> Span<'static> {
    if width == 0 {
        return Span::raw(String::new());
    }
    let text_width = text.width();
    if text_width == width {
        return Span::raw(text.to_owned());
    }
    if text_width < width {
        return Span::raw(format!("{text}{}", " ".repeat(width - text_width)));
    }
    // One cell goes to the ellipsis.
    let mut budget = width.saturating_sub(1);
    let mut cut = String::new();
    for ch in text.chars() {
        let char_width = ch.width().unwrap_or(0);
        if char_width > budget {
            break;
        }
        budget -= char_width;
        cut.push(ch);
    }
    cut.push('…');
    Span::raw(cut)
}

/// The price a model charges per million input or output tokens, the
/// catalog's own spelling.
fn price_cell(model: &ModelCard, input: bool) -> String {
    let Some(pricing) = model.pricing else {
        return String::new();
    };
    let price = if input {
        pricing.input_per_mtok
    } else {
        pricing.output_per_mtok
    };
    format!("${price:.2}")
}

/// The capabilities a model declares, one glyph each: vision today,
/// with the column sized to hold audio, tools, and whatever else the
/// listings grow. A model that declares none is blank -- a text-only
/// model wears no badge, and neither does one nobody described.
fn capability_cell(model: &ModelCard, icons: &crate::icons::Icons) -> String {
    let mut cell = String::new();
    if model.supports_vision == Some(true) {
        cell.push_str(icons.vision);
    }
    cell
}

/// One row of the list, laid out in the shared columns. The profile's
/// current model is named as such where a catalog row is it, so the
/// page says which one runs.
fn row_line(
    row: PickerRow<'_>,
    current: Option<(&str, &str)>,
    columns: &Columns,
    icons: &crate::icons::Icons,
    theme: &Theme,
) -> Line<'static> {
    let Some(model) = row.card() else {
        // The profile's own model, offered when the catalog names none.
        let line = Line::from(vec![
            Span::raw(format!("{}/", row.provider())).fg(theme.text3),
            Span::raw(row.id().to_owned()).bold(),
            Span::raw("  · in use").fg(theme.ok),
        ]);
        return truncate_line(line, columns.model);
    };
    let in_use = current.is_some_and(|(provider, id)| provider == row.provider() && id == row.id());
    model_row_marked(row.provider(), model, in_use, icons, theme, columns)
}

/// A catalog row on its own, for the tests that check a cell's text.
#[cfg(test)]
pub(crate) fn model_row(
    provider: &str,
    model: &ModelCard,
    icons: &crate::icons::Icons,
    theme: &Theme,
    width: usize,
) -> Line<'static> {
    let columns = Columns::of(&[(provider, model)], None, icons, width);
    model_row_marked(provider, model, false, icons, theme, &columns)
}

/// A catalog row, marked when the profile already runs it: the model
/// cell pads to its column, the in-use mark riding inside it, and every
/// number cell pads to its own, so the columns stay in line from row to
/// row and from pane to pane.
fn model_row_marked(
    provider: &str,
    model: &ModelCard,
    in_use: bool,
    icons: &crate::icons::Icons,
    theme: &Theme,
    columns: &Columns,
) -> Line<'static> {
    let mut spans = model_cell(provider, &model.id, in_use, theme, columns.model);
    for (width, cell) in [
        (columns.input, price_cell(model, true)),
        (columns.output, price_cell(model, false)),
        (columns.capabilities, capability_cell(model, icons)),
        (columns.ctx, format_tokens(model.input_token_limit)),
    ] {
        let Some(width) = width else { continue };
        spans.push(Span::raw(" ".repeat(COLUMN_GAP)));
        spans.push(pad(&cell, width).fg(theme.text3));
    }
    // Whatever overflowed a column that could not be cut (a pane
    // narrower than its own headers) stops at the edge.
    truncate_line(Line::from(spans), columns.model + columns.fixed())
}

/// The model column: `provider/` dim, the id bright, the in-use mark
/// after it, padded to the column's width.
fn model_cell(
    provider: &str,
    id: &str,
    in_use: bool,
    theme: &Theme,
    width: usize,
) -> Vec<Span<'static>> {
    let prefix = format!("{provider}/");
    // The id outranks the mark: a cell too narrow for both names the
    // model, and says "in use" only where there is room left over.
    let mark = if in_use && prefix.width() + id.width() + "  · in use".width() <= width {
        "  · in use"
    } else {
        ""
    };
    let used = prefix.width() + mark.width();
    let id = pad(id, width.saturating_sub(used));
    let mut spans = vec![Span::raw(prefix).fg(theme.text3), id.bold()];
    if in_use {
        spans.push(Span::raw(mark).fg(theme.ok));
    }
    spans
}

/// The status line: the filter and its caret while the picker takes
/// keys, the row count, the providers that went missing, the hints.
/// Narrow panes lose the tail first; the count stays.
fn footer_line(picker: &ModelPicker, chrome: &PickerChrome<'_>, theme: &Theme) -> Line<'static> {
    let mut spans = Vec::new();
    if picker.status == ModelPickerStatus::Loading && !picker.rows().is_empty() {
        // Rows are up while the rest of the catalog is out: the spinner
        // and the count stand where the filter box will.
        spans.push(
            Span::raw(super::status::spinner_char(picker.loading_for()).to_string())
                .fg(theme.accent_text),
        );
        spans.push(Span::raw(" loading…  ").fg(theme.text3));
        spans.push(Span::raw(format!("{} models", picker.row_count())).fg(theme.text3));
    } else if picker.status == ModelPickerStatus::Ready {
        // The filter line is always drawn: a picker whose search box
        // appears only once it takes keys reads as a plain list.
        spans.push(Span::raw("filter: ").fg(theme.text3));
        if picker.filter.is_empty() && !chrome.focused {
            if !chrome.prompt.is_empty() {
                spans.push(Span::raw(chrome.prompt.to_owned()).fg(theme.text3));
            }
        } else {
            spans.push(Span::raw(picker.filter.clone()).fg(theme.text).bold());
            if chrome.focused {
                spans.push(Span::raw("▌").fg(theme.accent_text).bold());
            }
        }
        spans.push(Span::raw("  "));
        spans.push(Span::raw(format!("{} models", picker.row_count())).fg(theme.text3));
    }
    // The providers that went missing are named in every state: a dead
    // one must never look like an empty catalog, whether the batch has
    // settled or is still out. A timeout is unavailable, not merely
    // empty; the red names the dead one while the live catalogs stay.
    if !picker.unavailable.is_empty() {
        spans.push(
            Span::raw(format!(
                "  · unavailable: {}",
                picker.unavailable.join(", ")
            ))
            .fg(theme.error),
        );
    }
    // A provider whose fetch failed while others answered: its absence
    // from the list must read as an error, not as an empty catalog.
    if !picker.failed.is_empty() {
        spans.push(
            Span::raw(format!("  · no models from {}", picker.failed.join(", "))).fg(theme.warn),
        );
    }
    // Hints name keys, and only the ones that still answer: the host's
    // line for a settled batch, what a list of landed rows answers while
    // more are out, the escape alone when there is nothing to pick. An
    // empty line is the host drawing its own, and it asks the picker for
    // the same state.
    let hints = if chrome.hints.is_empty() {
        ""
    } else {
        picker.state_hints(chrome.hints)
    };
    if !hints.is_empty() {
        spans.push(Span::raw("  "));
        spans.push(Span::raw(hints.to_owned()).fg(theme.text3));
    }
    Line::from(spans)
}

/// The full-screen picker over the whole terminal: the shared panel
/// chrome and the widget inside it, `/switch`'s screen.
pub(crate) fn render_picker(
    frame: &mut Frame,
    screen: &PickerScreen,
    icons: &crate::icons::Icons,
    theme: &Theme,
) {
    let area = frame.area();
    let block = super::chrome::panel(Some("switch model for this session"), theme);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    // One column of air inside the panel's border, and a row under its
    // title line.
    let inner = Rect {
        x: inner.x + 1,
        y: inner.y + 1,
        width: inner.width.saturating_sub(2),
        height: inner.height.saturating_sub(1),
    };
    render_model_picker(
        frame,
        inner,
        &screen.picker,
        icons,
        theme,
        &PickerChrome {
            title: None,
            hints: "type to filter · enter choose · alt-a all · esc back",
            focused: true,
            prompt: "",
        },
    );
}

/// A line cut to `width` display cells, an ellipsis taking the last
/// cell. Styles survive the cut; a line that already fits passes
/// through.
fn truncate_line(line: Line<'static>, width: usize) -> Line<'static> {
    let total: usize = line.spans.iter().map(|span| span.content.width()).sum();
    if total <= width || width == 0 {
        return line;
    }
    let mut budget = width - 1;
    let mut spans = Vec::new();
    for span in line.spans {
        if budget == 0 {
            break;
        }
        let mut piece = String::new();
        for ch in span.content.chars() {
            let char_width = ch.width().unwrap_or(0);
            if char_width > budget {
                budget = 0;
                break;
            }
            budget -= char_width;
            piece.push(ch);
        }
        if !piece.is_empty() {
            spans.push(Span::styled(piece, span.style));
        }
    }
    spans.push(Span::raw("…"));
    Line::from(spans)
}
