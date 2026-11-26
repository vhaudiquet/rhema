//! The model picker: the catalog rows, the filter, the selection, and
//! the keys. It draws nothing; [`crate::ui::model_picker`] renders it
//! into the rect its host hands it, so `/model`, `/switch`, the
//! onboarding wizard's model step, and the settings' Providers page all
//! show one widget with one set of semantics.

use std::collections::{BTreeMap, HashSet};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use rhema_provider::ModelCard;

/// How many release dates one family keeps: its current lineup and the
/// generation before it. Models sharing a date stay together.
const KEEP_RELEASES: usize = 2;

/// How old a release can be before the picker hides it, in days.
const MAX_AGE_DAYS: i32 = 365;

/// One pickable model, tagged with its provider's config name and the
/// instance it came from: the row prefix, the filter's provider match,
/// and the switch target on choose. A profile may hold several logins,
/// including several of one provider, so a catalog spans both.
#[derive(Debug, Clone, PartialEq)]
pub struct CatalogModel {
    /// The provider's config name (`google`, `openrouter`, ...).
    pub provider: &'static str,
    /// The instance's name in the profile's registry.
    pub instance: Option<String>,
    /// The normalized model.
    pub model: ModelCard,
}

/// The model the profile already runs, offered as a row when the
/// catalog names none: a rerun's `keep current`, the answer that needs
/// no network round trip.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CurrentModel {
    /// The provider's config name.
    pub provider: String,
    /// The instance the model came from, when the profile named one.
    pub instance: Option<String>,
    /// The model id.
    pub id: String,
}

/// Where the picker stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelPickerStatus {
    /// The model list is being fetched.
    Loading,
    /// Models are available.
    Ready,
    /// The fetch failed; the text is the provider's error. `r` retries.
    Error(String),
}

/// What one picker keypress means.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelPickerAction {
    /// Nothing to do.
    None,
    /// The highlighted model was chosen, with its provider's config name
    /// and the instance it came from: a pick from another provider or
    /// instance switches the session's backend.
    Choose {
        /// The provider's config name.
        provider: String,
        /// The instance's name in the profile.
        instance: Option<String>,
        /// The model's id.
        id: String,
    },
    /// Leave the picker: back to the chat, or back to the page that
    /// embedded it. The host decides.
    Back,
    /// Retry the failed model fetch.
    Retry,
}

/// One row the picker draws, in display order: the pinned current model
/// first when the catalog is empty, then the filtered catalog.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PickerRow<'a> {
    /// The model the profile already runs.
    Current(&'a CurrentModel),
    /// A catalog entry.
    Catalog(&'a CatalogModel),
}

impl<'a> PickerRow<'a> {
    /// The provider's config name.
    pub fn provider(&self) -> &'a str {
        match self {
            PickerRow::Current(current) => &current.provider,
            PickerRow::Catalog(entry) => entry.provider,
        }
    }

    /// The instance the row came from, when one is named.
    pub fn instance(&self) -> Option<&'a str> {
        match self {
            PickerRow::Current(current) => current.instance.as_deref(),
            PickerRow::Catalog(entry) => entry.instance.as_deref(),
        }
    }

    /// The model id.
    pub fn id(&self) -> &'a str {
        match self {
            PickerRow::Current(current) => &current.id,
            PickerRow::Catalog(entry) => &entry.model.id,
        }
    }

    /// The catalog card behind the row; the pinned current model has
    /// none, so the renderer draws it without price or context.
    pub fn card(&self) -> Option<&'a ModelCard> {
        match self {
            PickerRow::Current(_) => None,
            PickerRow::Catalog(entry) => Some(&entry.model),
        }
    }
}

/// The model picker: text chat models across every logged-in provider,
/// sorted by provider then id, a type-to-filter prompt, and one
/// highlighted row. Older versions stay hidden until `ctrl-a` asks for
/// them. The host owns the rect, the title, and what a pick means; this
/// type owns the rows and the keys.
#[derive(Debug)]
pub struct ModelPicker {
    /// The models to pick from (text chat only).
    pub models: Vec<CatalogModel>,
    /// Indices into [`Self::models`] of the rows the age and family
    /// rules hide: a family's releases older than its newest two dates,
    /// and anything more than a year old.
    hidden: HashSet<usize>,
    /// Days since 1970-01-01 the age rule measures against.
    today: i32,
    /// Whether `hidden` is ignored: the `ctrl-a` toggle.
    pub show_all: bool,
    /// The profile's stored model, offered when [`Self::models`] is
    /// empty and hidden as soon as the catalog names anything.
    pub current: Option<CurrentModel>,
    /// Providers whose fetch timed out while others answered; the
    /// footer names them as unavailable, in the error color.
    pub unavailable: Vec<&'static str>,
    /// Providers whose fetch failed with their own error while others
    /// succeeded; the footer names them so a missing catalog is never
    /// silent.
    pub failed: Vec<&'static str>,
    /// The filter text; printable keys extend it without any prefix.
    pub filter: String,
    /// Index into [`Self::rows`] of the highlighted row.
    pub selected: usize,
    /// Current status.
    pub status: ModelPickerStatus,
    /// When the loading line went up: its spinner steps off this clock,
    /// the redraw loop turning the frame. Set by [`Self::start_loading`]
    /// and never cleared, so a picker that never loaded still spins from
    /// its own start.
    pub loading_since: Option<std::time::Instant>,
}

impl ModelPicker {
    /// A picker over `models` (text chat models only, sorted by provider
    /// then id), initially unfiltered, with older versions hidden.
    pub fn new(models: Vec<CatalogModel>) -> Self {
        let mut picker = Self {
            models: Vec::new(),
            hidden: HashSet::new(),
            today: today(),
            show_all: false,
            current: None,
            unavailable: Vec::new(),
            failed: Vec::new(),
            filter: String::new(),
            selected: 0,
            status: ModelPickerStatus::Ready,
            loading_since: None,
        };
        picker.install(models);
        picker
    }

    /// The same picker with the age rule measured from `today`, the test
    /// seam that keeps the one-year cutoff out of the wall clock.
    #[cfg(test)]
    pub fn with_today(mut self, today: i32) -> Self {
        self.today = today;
        self.hidden = hidden_rows(&self.models, today);
        self.clamp_selection();
        self
    }

    /// Enters the loading state, stamping the clock its spinner steps on.
    pub fn start_loading(&mut self) {
        self.status = ModelPickerStatus::Loading;
        self.loading_since = Some(std::time::Instant::now());
    }

    /// Whether a pick can land right now: the rows on screen are chosen
    /// from, whether the batch has settled or not.
    pub fn pickable(&self) -> bool {
        self.status == ModelPickerStatus::Ready
            || (self.status == ModelPickerStatus::Loading && !self.rows().is_empty())
    }

    /// The keys that still answer, named by state: the host's own line
    /// once the batch settled, what a list of landed rows answers while
    /// more are out, and the escape alone when there is nothing to pick.
    pub fn state_hints<'a>(&self, ready: &'a str) -> &'a str {
        if self.status == ModelPickerStatus::Ready {
            ready
        } else if self.pickable() {
            "enter choose · ctrl-a all · esc back"
        } else {
            "esc back"
        }
    }

    /// How long the loading line has been up: the spinner's phase, zero
    /// on a picker that never stamped a start.
    pub fn loading_for(&self) -> std::time::Duration {
        self.loading_since
            .map(|started| started.elapsed())
            .unwrap_or_default()
    }

    /// The same picker with a pinned current model: the row a rerun
    /// offers until a catalog lands.
    pub fn with_current(mut self, current: Option<CurrentModel>) -> Self {
        self.current = current;
        self
    }

    /// The models this coding agent can drive: chat-capable rows. The
    /// adapters only hand out text chat models, the id markers that
    /// decide "text" are per-provider catalog knowledge, so this is just
    /// the defensive line against a row without the chat marker.
    fn usable(model: &ModelCard) -> bool {
        model.supports_chat()
    }

    /// Replaces the model list (scoped, re-sorted, and re-judged) and
    /// clamps the selection.
    pub fn set_models(&mut self, models: Vec<CatalogModel>) {
        self.install(models);
    }

    /// Filters to chat-capable rows, sorts, judges the age and family
    /// rules, and clamps the selection.
    fn install(&mut self, mut models: Vec<CatalogModel>) {
        models.retain(|entry| Self::usable(&entry.model));
        models.sort_by(|a, b| a.provider.cmp(b.provider).then(a.model.id.cmp(&b.model.id)));
        self.hidden = hidden_rows(&models, self.today);
        self.models = models;
        self.clamp_selection();
    }

    /// Replaces the pinned current model, the row a profile switch or a
    /// fresh login changes.
    pub fn set_current(&mut self, current: Option<CurrentModel>) {
        self.current = current;
        self.clamp_selection();
    }

    /// The models matching the filter: the rows the age and family
    /// rules hide are left out unless [`Self::show_all`], and the rest
    /// match a case-insensitive substring of `provider/id`, the
    /// instance name, or the display name, in the sorted order.
    pub fn filtered(&self) -> Vec<&CatalogModel> {
        let needle = self.filter.to_lowercase();
        self.models
            .iter()
            .enumerate()
            .filter(|(index, _)| self.show_all || !self.hidden.contains(index))
            .map(|(_, entry)| entry)
            .filter(|entry| {
                let mut haystack = format!("{}/{}", entry.provider, entry.model.id);
                if let Some(instance) = &entry.instance {
                    haystack.push_str(&format!(" {instance}"));
                }
                needle.is_empty()
                    || haystack.to_lowercase().contains(&needle)
                    || entry.model.display_name.to_lowercase().contains(&needle)
            })
            .collect()
    }

    /// The whole catalog, the search and age filters aside: what a
    /// table's columns are measured over, so narrowing the rows does not
    /// resize the table and a "no match" list keeps the titles that name
    /// its columns.
    pub fn catalog(&self) -> &[CatalogModel] {
        &self.models
    }

    /// The rows in display order: the pinned current model (while no
    /// filter narrows the list and no catalog arrived), then the
    /// filtered catalog.
    pub fn rows(&self) -> Vec<PickerRow<'_>> {
        let mut rows = Vec::new();
        if self.filter.is_empty()
            && self.models.is_empty()
            && let Some(current) = &self.current
        {
            rows.push(PickerRow::Current(current));
        }
        rows.extend(self.filtered().into_iter().map(PickerRow::Catalog));
        rows
    }

    /// How many rows the list holds.
    pub fn row_count(&self) -> usize {
        self.rows().len()
    }

    /// The row under the cursor, the one Enter picks.
    pub fn selected_row(&self) -> Option<PickerRow<'_>> {
        self.rows().into_iter().nth(self.selected)
    }

    fn clamp_selection(&mut self) {
        let count = self.row_count();
        if count == 0 {
            self.selected = 0;
        } else if self.selected >= count {
            self.selected = count - 1;
        }
    }

    /// Handles one keypress. Typing filters by default: printable keys
    /// extend the filter, arrows navigate, enter picks. Esc always
    /// leaves, the filter is transient, nothing to clear first. `r` stays
    /// a filter character on purpose: models are named (qwen).
    pub fn handle_key(&mut self, key: KeyEvent) -> ModelPickerAction {
        // Ctrl-A toggles the hidden rows from any state; Alt-A does too,
        // for terminals where Option sends Meta. A plain `a` stays a
        // filter character.
        if matches!(key.code, KeyCode::Char('a' | 'A'))
            && (key.modifiers.contains(KeyModifiers::CONTROL)
                || key.modifiers.contains(KeyModifiers::ALT))
        {
            self.show_all = !self.show_all;
            self.clamp_selection();
            return ModelPickerAction::None;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            return ModelPickerAction::None;
        }
        if self.status == ModelPickerStatus::Ready {
            match key.code {
                KeyCode::Char(c) => {
                    self.filter.push(c);
                    self.clamp_selection();
                    return ModelPickerAction::None;
                }
                KeyCode::Backspace => {
                    self.filter.pop();
                    self.clamp_selection();
                    return ModelPickerAction::None;
                }
                KeyCode::Up => {
                    self.selected = self.selected.saturating_sub(1);
                    return ModelPickerAction::None;
                }
                KeyCode::Down => {
                    self.selected = (self.selected + 1).min(self.row_count().saturating_sub(1));
                    return ModelPickerAction::None;
                }
                KeyCode::Enter => return self.choose(),
                KeyCode::Esc => return ModelPickerAction::Back,
                _ => return ModelPickerAction::None,
            }
        }
        // A fetch still out over rows that already landed: the rows are
        // real, so walking and picking them works; the filter box is not
        // drawn, so typing waits for the batch to settle.
        if self.status == ModelPickerStatus::Loading && !self.rows().is_empty() {
            return match key.code {
                KeyCode::Up => {
                    self.selected = self.selected.saturating_sub(1);
                    ModelPickerAction::None
                }
                KeyCode::Down => {
                    self.selected = (self.selected + 1).min(self.row_count().saturating_sub(1));
                    ModelPickerAction::None
                }
                KeyCode::Enter => self.choose(),
                KeyCode::Esc => ModelPickerAction::Back,
                _ => ModelPickerAction::None,
            };
        }
        // Loading or failed: only retry and leave are meaningful.
        match key.code {
            KeyCode::Char('r') if matches!(self.status, ModelPickerStatus::Error(_)) => {
                self.start_loading();
                ModelPickerAction::Retry
            }
            KeyCode::Esc => ModelPickerAction::Back,
            _ => ModelPickerAction::None,
        }
    }

    /// Enter: choose the highlighted row; an empty list chooses nothing.
    fn choose(&mut self) -> ModelPickerAction {
        // The caller decides whether a pick is allowed: the rows on
        // screen are the truth, whether the batch has settled or not.
        if self.selected_row().is_none() {
            return ModelPickerAction::None;
        }
        match self.rows().get(self.selected) {
            Some(row) => ModelPickerAction::Choose {
                provider: row.provider().to_owned(),
                instance: row.instance().map(str::to_owned),
                id: row.id().to_owned(),
            },
            None => ModelPickerAction::None,
        }
    }
}

/// Days since 1970-01-01, UTC: the clock the age rule reads.
fn today() -> i32 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| (elapsed.as_secs() / 86_400) as i32)
        .unwrap_or(0)
}

/// A `YYYY-MM-DD` release date as days since 1970-01-01. A malformed
/// or missing date answers `None`, and the row stays visible.
fn day_of(date: &str) -> Option<i32> {
    let mut parts = date.split('-');
    let year = parts.next()?.parse::<i32>().ok()?;
    let month = parts.next()?.parse::<u8>().ok()?;
    let day = parts.next()?.parse::<u8>().ok()?;
    if parts.next().is_some() || !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    Some(datealgo::date_to_rd((year, month, day)))
}

/// The indices the picker hides: a family's rows older than its newest
/// two release dates, and any row more than [`MAX_AGE_DAYS`] old. A row
/// without a usable date stays; the family rule only applies where the
/// catalog names a family.
fn hidden_rows(models: &[CatalogModel], today: i32) -> HashSet<usize> {
    let mut releases: BTreeMap<&str, Vec<i32>> = BTreeMap::new();
    let mut days: Vec<Option<i32>> = Vec::with_capacity(models.len());
    for entry in models {
        let day = entry.model.release_date.as_deref().and_then(day_of);
        days.push(day);
        if let (Some(family), Some(day)) = (entry.model.family.as_deref(), day) {
            releases.entry(family).or_default().push(day);
        }
    }
    let mut newest: BTreeMap<&str, Vec<i32>> = BTreeMap::new();
    for (family, mut dates) in releases {
        dates.sort_unstable();
        dates.dedup();
        dates.reverse();
        dates.truncate(KEEP_RELEASES);
        newest.insert(family, dates);
    }
    models
        .iter()
        .enumerate()
        .filter_map(|(index, entry)| {
            let day = days[index]?;
            let stale = today.saturating_sub(day) > MAX_AGE_DAYS;
            let retired = entry
                .model
                .family
                .as_deref()
                .and_then(|family| newest.get(family))
                .is_some_and(|kept| !kept.contains(&day));
            (stale || retired).then_some(index)
        })
        .collect()
}
