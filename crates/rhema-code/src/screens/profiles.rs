//! The profiles screen: its rows over the registry, the actions they carry, and the screen state.

use super::*;

/// One row of the profile picker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileRow {
    /// The profile's name.
    pub name: String,
    /// The provider this profile is set to, when configured.
    pub provider: Option<String>,
    /// The model this profile is set to, when configured.
    pub model: Option<String>,
    /// The profile's display color, when configured.
    pub color: Option<String>,
    /// Whether this profile's backend has usable credentials.
    pub usable: bool,
    /// Whether this profile is the one the session is running on.
    pub active: bool,
}

/// What one profile-picker keypress means.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProfileAction {
    /// Nothing to do.
    None,
    /// Switch the session to the named profile.
    Switch(String),
    /// Return to the chat.
    BackToChat,
}

/// The config's profiles as rows, sorted by name: the profile picker's
/// list and the settings Profiles page's snapshot.
pub fn profile_rows(
    profiles: &BTreeMap<String, rhema_config::Profile>,
    active: Option<&str>,
    secrets: Option<&dyn rhema_config::secrets::SecretStore>,
) -> Vec<ProfileRow> {
    let mut entries: Vec<ProfileRow> = profiles
        .iter()
        .map(|(name, profile)| ProfileRow {
            name: name.clone(),
            provider: profile.provider.clone(),
            model: profile.model.clone(),
            color: profile.color.clone(),
            usable: profile.has_credentials(secrets),
            active: Some(name.as_str()) == active,
        })
        .collect();
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    entries
}

/// The profile picker: one highlighted row, arrows to move, enter to
/// switch, esc back to the chat. Profiles are few, so there is no filter.
#[derive(Debug)]
pub struct ProfileScreen {
    /// The rows, sorted by name.
    pub entries: Vec<ProfileRow>,
    /// Index of the highlighted row.
    pub selected: usize,
}

impl ProfileScreen {
    /// A picker over the config's profiles. `active` marks the row the
    /// session is currently running on.
    pub fn new(
        profiles: &BTreeMap<String, rhema_config::Profile>,
        active: Option<&str>,
        secrets: Option<&dyn rhema_config::secrets::SecretStore>,
    ) -> Self {
        Self {
            entries: profile_rows(profiles, active, secrets),
            selected: 0,
        }
    }

    /// Handles one keypress.
    pub fn handle_key(&mut self, key: KeyEvent) -> ProfileAction {
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            return ProfileAction::None;
        }
        match key.code {
            KeyCode::Down => {
                if !self.entries.is_empty() {
                    self.selected = (self.selected + 1).min(self.entries.len() - 1);
                }
                ProfileAction::None
            }
            KeyCode::Up => {
                self.selected = self.selected.saturating_sub(1);
                ProfileAction::None
            }
            KeyCode::Enter => self.switch(),
            KeyCode::Esc => ProfileAction::BackToChat,
            _ => ProfileAction::None,
        }
    }

    /// Enter: switch to the highlighted profile.
    fn switch(&mut self) -> ProfileAction {
        match self.entries.get(self.selected) {
            Some(row) => ProfileAction::Switch(row.name.clone()),
            None => ProfileAction::None,
        }
    }
}

// --- Sandbox ---
