//! The first-run wizard: a left rail with the logo and the steps, a
//! body with the current step, one footer of hints — the same chrome
//! the settings screen wears.
//!
//! The steps: welcome, the default profile (the config's existing
//! profiles, or a new one), a provider login (the settings'
//! add-instance dialog reused inline), a model pick from the fresh
//! login's catalog, the color palette (applied live as the rows move),
//! the bundled skills' on/off toggles, and a finish step that points at
//! `/settings`. Steps that need the app — creating or selecting a
//! profile, validating a credential, persisting a model choice —
//! return [`OnboardingAction`]s and wait; the screen never touches the
//! store itself. Esc walks back one step; at the welcome step it quits,
//! and an unfinished wizard reopens on the next start (the config's
//! `onboarded` marker stays `false` until the finish step's Enter).

use crate::screens::{
    AddInstanceDialog, AddProfileDialog, CatalogModel, DialogAction, ProfileDialogAction, SkillRow,
};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::collections::BTreeMap;

/// The wizard's steps, in order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnboardingStep {
    /// The logo and the wordmark; Enter begins, Esc quits.
    Welcome,
    /// The default profile: an existing one or a new name.
    Profile,
    /// A provider login, through the add-instance dialog.
    Login,
    /// A model from the fresh login's catalog.
    Model,
    /// The color palette, previewed live.
    Appearance,
    /// The bundled skills' on/off toggles.
    Skills,
    /// The summary and the way into `/settings`.
    Finish,
}

impl OnboardingStep {
    /// Every step, in rail order.
    pub const ALL: &'static [OnboardingStep] = &[
        OnboardingStep::Welcome,
        OnboardingStep::Profile,
        OnboardingStep::Login,
        OnboardingStep::Model,
        OnboardingStep::Appearance,
        OnboardingStep::Skills,
        OnboardingStep::Finish,
    ];

    /// The rail's label for this step.
    pub fn label(self) -> &'static str {
        match self {
            OnboardingStep::Welcome => "Welcome",
            OnboardingStep::Profile => "Profile",
            OnboardingStep::Login => "Provider",
            OnboardingStep::Model => "Model",
            OnboardingStep::Appearance => "Colors",
            OnboardingStep::Skills => "Skills",
            OnboardingStep::Finish => "Finish",
        }
    }
}

/// What one wizard keypress asks the app to do. Everything the screen
/// can decide alone (step order, row movement, the dialog's own stage
/// machine) it does; these are the moves that touch the store, the
/// network, or the session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OnboardingAction {
    /// Nothing to do.
    None,
    /// The dialog submitted a credential: validate it through the model
    /// fetch, exactly the settings' login path.
    Validate,
    /// The dialog named a flow provider: spawn its interactive flow.
    StartFlow,
    /// The dialog closed while a flow ran: drop the flow task.
    CancelFlow,
    /// A model row was picked: persist it as the profile's model and
    /// build the agent, then the step advances. The provider is the
    /// config name — a catalog row's is `&'static`, the keep-current
    /// row's comes from the stored profile.
    ChooseModel {
        /// The provider's config name.
        provider: String,
        /// The instance's name in the profile.
        instance: Option<String>,
        /// The model's id.
        id: String,
    },
    /// A skills row was toggled.
    ToggleSkill(String),
    /// A palette row was moved onto: apply it live.
    SetPalette(crate::palette::Palette),
    /// The profile step selected an existing profile as the default.
    SelectProfile(String),
    /// The add-a-profile input submitted a fresh name: create it and
    /// make it the default.
    CreateProfile(String),
    /// The finish step's Enter: mark the wizard done and land in the
    /// chat.
    Complete,
    /// Leave the app (Esc on the welcome step).
    Quit,
}

/// The wizard's state: the current step, the row within it, the login
/// and add-profile inputs while they run, and the snapshots the steps
/// read.
#[derive(Debug)]
pub struct OnboardingScreen {
    /// The step on screen.
    pub step: OnboardingStep,
    /// The selected row within the step (a profile, the login rows, a
    /// model, a palette, a skill).
    pub row: usize,
    /// The login dialog while the Login step runs one.
    pub dialog: Option<AddInstanceDialog>,
    /// The model rows, filled when the login's catalog lands.
    pub models: Vec<CatalogModel>,
    /// The model the wizard landed on, for the finish summary.
    pub chosen_model: Option<String>,
    /// The provider the wizard logged into, for the finish summary.
    pub chosen_provider: Option<String>,
    /// The skills the Skills step toggles: the same rows the settings
    /// page shows, sources and on/off states included.
    pub skills: Vec<SkillRow>,
    /// The config's profiles, sorted by name: the Profile step's rows,
    /// refreshed when a profile is created or the wizard moves to
    /// another one.
    pub profiles: BTreeMap<String, rhema_config::Profile>,
    /// The chosen default profile's name, refreshed with [`Self::stored`]
    /// when the Profile step lands on another one.
    pub default_profile: String,
    /// The add-a-profile input while the Profile step types a new name.
    pub profile_dialog: Option<AddProfileDialog>,
    /// Whether a usable backend already serves (an environment key, or
    /// the login just landed): the login rows may be skipped past.
    pub backend_ready: bool,
    /// The last step failure the app reported (a model choice that
    /// could not build, a config write refused); cleared with the step.
    pub error: Option<String>,
    /// The profile snapshot the add-instance dialog dedupes names
    /// against, refreshed when the Profile step lands on another
    /// profile or a login lands.
    pub stored: rhema_config::Profile,
}

impl OnboardingScreen {
    /// A wizard at the welcome step. The skills, the profile registry,
    /// and the default profile's snapshot come from the opener.
    pub fn new(
        skills: Vec<SkillRow>,
        default_profile: String,
        profiles: BTreeMap<String, rhema_config::Profile>,
        backend_ready: bool,
        stored: rhema_config::Profile,
    ) -> Self {
        Self {
            step: OnboardingStep::Welcome,
            row: 0,
            dialog: None,
            models: Vec::new(),
            chosen_model: None,
            chosen_provider: None,
            skills,
            profiles,
            default_profile,
            profile_dialog: None,
            backend_ready,
            error: None,
            stored,
        }
    }

    /// The Profile step landed on `name`: it becomes the default, the
    /// snapshots move with it, and the wizard walks on to its login
    /// step. A fresh login's catalog and the previous profile's model
    /// choice do not carry over.
    pub fn select_profile(
        &mut self,
        name: String,
        stored: rhema_config::Profile,
        profiles: BTreeMap<String, rhema_config::Profile>,
    ) {
        self.default_profile = name;
        self.stored = stored;
        self.profiles = profiles;
        self.models.clear();
        self.chosen_model = None;
        self.chosen_provider = None;
        self.go(OnboardingStep::Login);
    }

    /// Moves to `step`, resetting the row selection — the rows move
    /// with the step — and dropping the last step's error with it.
    pub fn go(&mut self, step: OnboardingStep) {
        self.step = step;
        self.row = 0;
        self.error = None;
    }

    /// The row count of the step on screen.
    fn row_count(&self) -> usize {
        match self.step {
            OnboardingStep::Welcome | OnboardingStep::Finish => 0,
            // Add a login / skip for now.
            OnboardingStep::Login => {
                if self.dialog.is_some() {
                    0
                } else {
                    2
                }
            }
            OnboardingStep::Model => {
                self.models.len() + self.keep_current_model().is_some() as usize
            }
            // One row per profile, then the add row.
            OnboardingStep::Profile => {
                if self.profile_dialog.is_some() {
                    0
                } else {
                    self.profiles.len() + 1
                }
            }
            OnboardingStep::Appearance => crate::palette::Palette::ALL.len(),
            OnboardingStep::Skills => self.skills.len(),
        }
    }

    /// Moves the row selection, wrapping.
    fn move_row(&mut self, down: bool) {
        let count = self.row_count();
        if count == 0 {
            return;
        }
        let step = if down { 1 } else { -1 };
        self.row = ((self.row as isize + step).rem_euclid(count as isize)) as usize;
    }

    /// Handles one keypress. Esc walks back a step (quitting at the
    /// welcome step); Enter moves forward or acts; Space and `t` toggle
    /// a skill on the Skills step.
    pub fn handle_key(&mut self, key: KeyEvent) -> OnboardingAction {
        if self.profile_dialog.is_some() {
            let (action, name) = {
                let dialog = self.profile_dialog.as_mut().unwrap();
                (dialog.handle_key(key), dialog.name().to_owned())
            };
            return match action {
                ProfileDialogAction::None => OnboardingAction::None,
                ProfileDialogAction::Submit => {
                    self.profile_dialog = None;
                    OnboardingAction::CreateProfile(name)
                }
                ProfileDialogAction::Close => {
                    self.profile_dialog = None;
                    OnboardingAction::None
                }
            };
        }
        if let Some(dialog) = self.dialog.as_mut() {
            let action = dialog.handle_key(key);
            return match action {
                DialogAction::None => OnboardingAction::None,
                DialogAction::Validate => OnboardingAction::Validate,
                DialogAction::StartFlow => OnboardingAction::StartFlow,
                DialogAction::Close | DialogAction::CloseCancelFlow => {
                    self.dialog = None;
                    match action {
                        DialogAction::CloseCancelFlow => OnboardingAction::CancelFlow,
                        _ => OnboardingAction::None,
                    }
                }
            };
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            return OnboardingAction::None;
        }
        match self.step {
            OnboardingStep::Welcome => match key.code {
                KeyCode::Enter => {
                    self.go(OnboardingStep::Profile);
                    OnboardingAction::None
                }
                KeyCode::Esc => OnboardingAction::Quit,
                _ => OnboardingAction::None,
            },
            OnboardingStep::Profile => self.handle_profile_key(key),
            OnboardingStep::Login => self.handle_login_key(key),
            OnboardingStep::Model => self.handle_model_key(key),
            OnboardingStep::Appearance => self.handle_appearance_key(key),
            OnboardingStep::Skills => self.handle_skills_key(key),
            OnboardingStep::Finish => match key.code {
                KeyCode::Enter => OnboardingAction::Complete,
                KeyCode::Esc => {
                    self.go(OnboardingStep::Skills);
                    OnboardingAction::None
                }
                _ => OnboardingAction::None,
            },
        }
    }

    /// The login rows: add a login, or skip. The dialog, while it runs,
    /// took the keys above.
    fn handle_login_key(&mut self, key: KeyEvent) -> OnboardingAction {
        match key.code {
            KeyCode::Up | KeyCode::Down => {
                self.move_row(key.code == KeyCode::Down);
                OnboardingAction::None
            }
            KeyCode::Enter => match self.row {
                0 => {
                    self.dialog = Some(AddInstanceDialog::new(&self.stored));
                    OnboardingAction::None
                }
                _ => {
                    // Skipping straight past the model step: nothing was
                    // logged in, so there is no catalog to pick from.
                    self.go(OnboardingStep::Appearance);
                    OnboardingAction::None
                }
            },
            KeyCode::Esc => {
                self.go(OnboardingStep::Profile);
                OnboardingAction::None
            }
            _ => OnboardingAction::None,
        }
    }

    /// The model rows: a keep-current row when the profile already
    /// carries a model (a `rhema onboard` rerun), then the fresh
    /// login's catalog.
    fn handle_model_key(&mut self, key: KeyEvent) -> OnboardingAction {
        match key.code {
            KeyCode::Up | KeyCode::Down => {
                self.move_row(key.code == KeyCode::Down);
                OnboardingAction::None
            }
            KeyCode::Enter => {
                if self.keep_current_model().is_some() && self.row == 0 {
                    let (provider, instance, id) = self.keep_current_model().unwrap();
                    return OnboardingAction::ChooseModel {
                        provider,
                        instance,
                        id,
                    };
                }
                let offset = self.keep_current_model().is_some() as usize;
                match self.models.get(self.row - offset) {
                    Some(entry) => OnboardingAction::ChooseModel {
                        provider: entry.provider.to_owned(),
                        instance: entry.instance.clone(),
                        id: entry.model.id.clone(),
                    },
                    None => OnboardingAction::None,
                }
            }
            KeyCode::Esc => {
                self.go(OnboardingStep::Login);
                OnboardingAction::None
            }
            _ => OnboardingAction::None,
        }
    }

    /// The keep-current row of the Model step: the profile's stored
    /// model, when the catalog carries no fresh login's rows — the
    /// rerun path, where keeping is the common answer. `None` once a
    /// login populated the catalog: those rows are the real choices.
    pub fn keep_current_model(&self) -> Option<(String, Option<String>, String)> {
        if !self.models.is_empty() {
            return None;
        }
        let provider = self.stored.provider_name()?.to_owned();
        let model = self.stored.model.clone()?;
        Some((provider, self.stored.instance.clone(), model))
    }

    /// The profile rows: the config's profiles, then the add row.
    /// Enter on a profile selects it as the default and moves on; Enter
    /// on the add row opens the name input. The dialog, while it runs,
    /// took the keys above.
    fn handle_profile_key(&mut self, key: KeyEvent) -> OnboardingAction {
        match key.code {
            KeyCode::Up | KeyCode::Down => {
                self.move_row(key.code == KeyCode::Down);
                OnboardingAction::None
            }
            KeyCode::Enter => {
                let picked = self.profiles.keys().nth(self.row).cloned();
                match picked {
                    Some(name) => OnboardingAction::SelectProfile(name),
                    None => {
                        self.profile_dialog = Some(AddProfileDialog::new(&self.profiles));
                        OnboardingAction::None
                    }
                }
            }
            KeyCode::Esc => {
                self.go(OnboardingStep::Welcome);
                OnboardingAction::None
            }
            _ => OnboardingAction::None,
        }
    }

    /// The palette rows: moving selects and previews live, Enter keeps
    /// the choice and moves on.
    fn handle_appearance_key(&mut self, key: KeyEvent) -> OnboardingAction {
        let all = crate::palette::Palette::ALL;
        match key.code {
            KeyCode::Up | KeyCode::Down => {
                self.move_row(key.code == KeyCode::Down);
                OnboardingAction::SetPalette(all[self.row.min(all.len() - 1)])
            }
            KeyCode::Enter => {
                self.go(OnboardingStep::Skills);
                OnboardingAction::None
            }
            KeyCode::Esc => {
                self.go(OnboardingStep::Model);
                OnboardingAction::None
            }
            _ => OnboardingAction::None,
        }
    }

    /// The skills rows: Space or `t` toggles, Enter moves on.
    fn handle_skills_key(&mut self, key: KeyEvent) -> OnboardingAction {
        let name = || self.skills.get(self.row).map(|skill| skill.name.clone());
        match key.code {
            KeyCode::Up | KeyCode::Down => {
                self.move_row(key.code == KeyCode::Down);
                OnboardingAction::None
            }
            KeyCode::Char(' ' | 't') => match name() {
                Some(name) => OnboardingAction::ToggleSkill(name),
                None => OnboardingAction::None,
            },
            KeyCode::Enter => {
                self.go(OnboardingStep::Finish);
                OnboardingAction::None
            }
            KeyCode::Esc => {
                self.go(OnboardingStep::Appearance);
                OnboardingAction::None
            }
            _ => OnboardingAction::None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profiles() -> BTreeMap<String, rhema_config::Profile> {
        rhema_config::Settings::default().profiles
    }

    fn wizard() -> OnboardingScreen {
        OnboardingScreen::new(
            Vec::new(),
            "personal".to_owned(),
            profiles(),
            false,
            Default::default(),
        )
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::empty())
    }

    #[test]
    fn the_rail_walks_forward_on_enter_and_back_on_esc() {
        let mut screen = wizard();
        assert_eq!(
            screen.handle_key(key(KeyCode::Enter)),
            OnboardingAction::None
        );
        assert_eq!(screen.step, OnboardingStep::Profile);
        assert_eq!(
            screen.handle_key(key(KeyCode::Enter)),
            OnboardingAction::SelectProfile("personal".to_owned())
        );
        // The app lands the selection; the screen moves on with it.
        screen.select_profile("personal".to_owned(), Default::default(), profiles());
        assert_eq!(screen.step, OnboardingStep::Login);
        // The login rows: enter on the skip row walks past the model
        // step (no catalog exists to pick from).
        screen.handle_key(key(KeyCode::Down));
        assert_eq!(screen.row, 1);
        assert_eq!(
            screen.handle_key(key(KeyCode::Enter)),
            OnboardingAction::None
        );
        assert_eq!(screen.step, OnboardingStep::Appearance);
        // Esc walks back one step at a time.
        assert_eq!(screen.handle_key(key(KeyCode::Esc)), OnboardingAction::None);
        assert_eq!(screen.step, OnboardingStep::Model);
    }

    #[test]
    fn esc_at_the_welcome_step_quits() {
        let mut screen = wizard();
        assert_eq!(screen.handle_key(key(KeyCode::Esc)), OnboardingAction::Quit);
    }

    #[test]
    fn the_profile_step_offers_the_registry_then_an_add_row() {
        let mut screen = wizard();
        screen.go(OnboardingStep::Profile);
        assert_eq!(screen.row_count(), 3, "personal, work, and the add row");
        assert_eq!(
            screen.handle_key(key(KeyCode::Enter)),
            OnboardingAction::SelectProfile("personal".to_owned())
        );
        screen.handle_key(key(KeyCode::Down));
        assert_eq!(
            screen.handle_key(key(KeyCode::Enter)),
            OnboardingAction::SelectProfile("work".to_owned())
        );
        // The add row opens the name input instead.
        screen.handle_key(key(KeyCode::Down));
        assert_eq!(
            screen.handle_key(key(KeyCode::Enter)),
            OnboardingAction::None
        );
        assert!(screen.profile_dialog.is_some(), "the name input is up");
        assert_eq!(screen.row_count(), 0, "the dialog owns the keys");
        for c in "home".chars() {
            screen.handle_key(key(KeyCode::Char(c)));
        }
        assert_eq!(
            screen.handle_key(key(KeyCode::Enter)),
            OnboardingAction::CreateProfile("home".to_owned())
        );
        assert!(screen.profile_dialog.is_none(), "submit closes the input");
    }

    #[test]
    fn selecting_a_profile_moves_the_snapshots_and_drops_the_stale_catalog() {
        let mut screen = wizard();
        screen.models = vec![CatalogModel {
            provider: "google",
            instance: Some("google".into()),
            model: rhema_provider::ModelCard {
                id: "gemini-x".into(),
                display_name: "Gemini X".into(),
                description: String::new(),
                input_token_limit: None,
                output_token_limit: None,
                supports_vision: None,
                generation_methods: vec![],
                pricing: None,
            },
        }];
        screen.chosen_model = Some("gemini-x".into());
        let stored = rhema_config::Profile {
            provider: Some("anthropic".into()),
            model: Some("claude".into()),
            ..Default::default()
        };
        screen.select_profile("work".to_owned(), stored, profiles());
        assert_eq!(screen.default_profile, "work");
        assert_eq!(screen.step, OnboardingStep::Login);
        assert_eq!(screen.stored.provider.as_deref(), Some("anthropic"));
        assert!(screen.models.is_empty(), "another login's catalog is stale");
        assert!(screen.chosen_model.is_none());
    }

    #[test]
    fn the_skills_step_toggles_with_space_and_advances_with_enter() {
        let mut screen = OnboardingScreen::new(
            vec![
                SkillRow {
                    name: "commit-message".into(),
                    description: "commits".into(),
                    license: None,
                    source: crate::screens::SkillSource::Bundled,
                    enabled: true,
                },
                SkillRow {
                    name: "unslop".into(),
                    description: "reviews".into(),
                    license: Some("MIT".into()),
                    source: crate::screens::SkillSource::Bundled,
                    enabled: true,
                },
            ],
            "personal".into(),
            profiles(),
            false,
            Default::default(),
        );
        screen.go(OnboardingStep::Skills);
        assert_eq!(
            screen.handle_key(key(KeyCode::Char(' '))),
            OnboardingAction::ToggleSkill("commit-message".into())
        );
        screen.handle_key(key(KeyCode::Down));
        assert_eq!(
            screen.handle_key(key(KeyCode::Char('t'))),
            OnboardingAction::ToggleSkill("unslop".into())
        );
        assert_eq!(
            screen.handle_key(key(KeyCode::Enter)),
            OnboardingAction::None
        );
        assert_eq!(screen.step, OnboardingStep::Finish);
    }

    #[test]
    fn the_appearance_step_previews_the_palette_it_lands_on() {
        let mut screen = wizard();
        screen.go(OnboardingStep::Appearance);
        let all = crate::palette::Palette::ALL;
        assert_eq!(
            screen.handle_key(key(KeyCode::Down)),
            OnboardingAction::SetPalette(all[1])
        );
        // Wrapping off the top lands on the last palette.
        screen.go(OnboardingStep::Appearance);
        assert_eq!(
            screen.handle_key(key(KeyCode::Up)),
            OnboardingAction::SetPalette(all[all.len() - 1])
        );
    }

    #[test]
    fn the_model_step_chooses_the_highlighted_row() {
        let mut screen = wizard();
        screen.models = vec![CatalogModel {
            provider: "google",
            instance: Some("google".into()),
            model: rhema_provider::ModelCard {
                id: "gemini-x".into(),
                display_name: "Gemini X".into(),
                description: String::new(),
                input_token_limit: None,
                output_token_limit: None,
                supports_vision: None,
                generation_methods: vec![],
                pricing: None,
            },
        }];
        screen.go(OnboardingStep::Model);
        match screen.handle_key(key(KeyCode::Enter)) {
            OnboardingAction::ChooseModel {
                provider,
                instance,
                id,
            } => {
                assert_eq!(provider, "google");
                assert_eq!(instance.as_deref(), Some("google"));
                assert_eq!(id, "gemini-x");
            }
            other => panic!("expected a choose, got {other:?}"),
        }
    }

    #[test]
    fn a_rerun_offers_the_stored_model_as_keep_current() {
        let stored = rhema_config::Profile {
            provider: Some("google".into()),
            instance: Some("google".into()),
            model: Some("gemini-x".into()),
            ..Default::default()
        };
        let mut screen =
            OnboardingScreen::new(Vec::new(), "personal".into(), profiles(), true, stored);
        screen.go(OnboardingStep::Model);
        assert_eq!(screen.row_count(), 1, "the keep-current row alone");
        assert_eq!(
            screen.handle_key(key(KeyCode::Enter)),
            OnboardingAction::ChooseModel {
                provider: "google".into(),
                instance: Some("google".into()),
                id: "gemini-x".into(),
            }
        );
        // A fresh login's catalog replaces it: those rows are the
        // choices then.
        screen.models = vec![CatalogModel {
            provider: "google",
            instance: Some("google".into()),
            model: rhema_provider::ModelCard {
                id: "gemini-y".into(),
                display_name: "Gemini Y".into(),
                description: String::new(),
                input_token_limit: None,
                output_token_limit: None,
                supports_vision: None,
                generation_methods: vec![],
                pricing: None,
            },
        }];
        assert_eq!(screen.row_count(), 1);
        assert_eq!(
            screen.handle_key(key(KeyCode::Enter)),
            OnboardingAction::ChooseModel {
                provider: "google".into(),
                instance: Some("google".into()),
                id: "gemini-y".into(),
            }
        );
    }
}
