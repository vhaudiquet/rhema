//! Input handling: keys, clicks, paste, the chat line, and the
//! steering of a running turn. The router the screens feed lives
//! beside the handlers it calls.

use super::*;

/// The most bytes one paste may carry. A clipboard can hold anything;
/// the input is a prompt, not a file drop target.
const MAX_PASTE_BYTES: usize = 1024 * 1024;

/// Whether the key is the transcript's copy shortcut: Ctrl-C, with or
/// without Shift, as a terminal copies.
///
/// AltGr arrives as `CONTROL|ALT` and composes a character instead (on
/// AZERTY, `@` is AltGr+0), so a char carrying ALT is text, not a
/// shortcut. Ctrl-Shift-C needs no special case: the kitty alternate-key
/// report leaves the shifted `C`, and a terminal that keeps modifiers
/// sends `c` with `SHIFT`, both under `CONTROL`.
fn is_copy_key(key: KeyEvent) -> bool {
    key.modifiers.contains(KeyModifiers::CONTROL)
        && !key.modifiers.contains(KeyModifiers::ALT)
        && matches!(key.code, KeyCode::Char('c' | 'C'))
}

impl App {
    /// In-band decline for a slash input no table row matches.
    pub fn unknown_command(&mut self, name: &str) {
        self.transcript
            .push_notice(&format!("unknown command {name}; type / for commands"));
    }

    /// A notice into the transcript, styled by the transcript itself.
    pub fn push_notice(&mut self, text: &str) {
        self.transcript.push_notice(text);
    }

    /// Whether an open dialog owns the ctrl-v paste right now: the
    /// dialog family with a text line, the same set [`Self::on_paste`]
    /// feeds.
    pub(crate) fn dialog_takes_paste(&self) -> bool {
        match &self.screen {
            Screen::Settings(settings) => {
                settings.dialog.is_some()
                    || settings.add_profile.is_some()
                    || settings.mcp_dialog.is_some()
                    || settings.websearch_dialog.is_some()
                    || settings.decision_dialog.is_some()
            }
            Screen::Onboarding(wizard) => wizard.dialog.is_some(),
            _ => false,
        }
    }

    /// A bracketed paste landed: into the input, newlines included,
    /// pasting multi-line text must not fire submits. The add-instance
    /// dialog's credential line takes its own, single-line shape. A
    /// paste past `MAX_PASTE_BYTES` is cut there and noticed: the
    /// input (and the turn it would ride) has no other bound.
    pub fn on_paste(&mut self, text: &str) {
        if text.len() > MAX_PASTE_BYTES {
            let mut cut = MAX_PASTE_BYTES;
            while !text.is_char_boundary(cut) {
                cut -= 1;
            }
            let text = &text[..cut];
            self.push_notice(&format!(
                "paste truncated to {} MiB",
                MAX_PASTE_BYTES / (1024 * 1024)
            ));
            self.on_paste(text);
            return;
        }
        match &mut self.screen {
            Screen::Chat => self.chat.paste(text),
            Screen::Settings(settings) => {
                if let Some(dialog) = &mut settings.dialog {
                    dialog.paste(text);
                }
                if let Some(dialog) = &mut settings.add_profile {
                    dialog.paste(text);
                }
                if let Some(dialog) = &mut settings.mcp_dialog {
                    dialog.paste(text);
                }
                if let Some(dialog) = &mut settings.websearch_dialog {
                    dialog.paste(text);
                }
                if let Some(dialog) = &mut settings.decision_dialog {
                    dialog.paste(text);
                }
            }
            Screen::Onboarding(wizard) => {
                if let Some(dialog) = &mut wizard.dialog {
                    dialog.paste(text);
                }
            }
            _ => {}
        }
    }

    /// Scroll the history with the wheel: the same step as PageUp/PageDown
    /// (one wheel tick), so both controls move in lockstep. The wheel
    /// moves the rows a selection was measured against, so it clears it.
    pub fn on_mouse_scroll(&mut self, up: bool) {
        self.selection = None;
        if matches!(self.screen, Screen::Chat) {
            let step = crate::screens::ChatState::SCROLL_STEP as isize;
            self.chat.scroll_by(if up { step } else { -step });
        }
    }

    /// A left button went down: over the transcript it starts a text
    /// selection, the way dragging over a terminal does. Anywhere else
    /// it clears the previous selection; clicks still act on release.
    pub fn on_mouse_down(&mut self, x: u16, y: u16) {
        self.selection = None;
        self.rail_drag = false;
        if !matches!(self.screen, Screen::Chat)
            || self.turn.pending_approval.is_some()
            || self.confirm_quit
            || self.provider_followup.is_some()
        {
            return;
        }
        // The rail is a control before it is text: a press on it moves
        // the window and arms a drag, it never starts a selection.
        if let Some(rail) = self.transcript_view.rail
            && rail.area.contains(ratatui::layout::Position::new(x, y))
        {
            self.chat.scroll = rail.scroll_at(y);
            self.rail_drag = true;
            return;
        }
        if self.transcript_view.starts_at(x, y) {
            self.selection = Some(super::selection::Selection::new(
                (x, y),
                self.transcript_view.top,
            ));
        }
    }

    /// The drag moved: the selection follows the pointer, clamped into
    /// the transcript so a drag past an edge still selects the text it
    /// crossed.
    pub fn on_mouse_drag(&mut self, x: u16, y: u16) {
        if self.rail_drag {
            if let Some(rail) = self.transcript_view.rail {
                self.chat.scroll = rail.scroll_at(y);
            }
            return;
        }
        if self.selection.is_none() || self.transcript_view.rows.is_empty() {
            return;
        }
        let area = self.transcript_view.area;
        let last_row = self.transcript_view.rows.len().saturating_sub(1) as u16;
        let right = area.right().saturating_sub(1).max(area.x);
        let x = x.clamp(area.x, right);
        let y = y.clamp(area.y, area.y.saturating_add(last_row));
        if let Some(selection) = &mut self.selection {
            selection.extend((x, y));
        }
    }

    /// The left button came up: copies the text between the selection's
    /// cells, and keeps the highlight until a key, the wheel, or another
    /// click clears it. A release with no selection to consume returns
    /// `false`, so the caller still routes it as a click.
    pub fn finish_selection(&mut self, x: u16, y: u16) -> bool {
        if self.rail_drag {
            self.rail_drag = false;
            return true;
        }
        if self.selection.is_none() {
            return false;
        }
        self.on_mouse_drag(x, y);
        self.copy_selection();
        true
    }

    /// Copies the selected transcript text to the clipboard and keeps
    /// the selection, so the highlight stays and another press copies
    /// the same text. Returns whether there was text to copy.
    pub(crate) fn copy_selection(&mut self) -> bool {
        let Some(selection) = self.selection else {
            return false;
        };
        let text = super::selection::text(&self.transcript_view, selection);
        if text.is_empty() {
            return false;
        }
        crate::screens::clipboard::write(&text);
        true
    }

    /// A left click landed. The settings menu answers clicks, sidebar
    /// rows switch sections, page rows select-and-act, the add dialog's
    /// provider rows pick, everywhere else the keyboard rules. The hit
    /// rects are re-registered by every frame's render, so a click can
    /// only land on what was last drawn.
    pub fn on_click(&mut self, x: u16, y: u16) -> Vec<Effect> {
        if matches!(self.screen, Screen::Onboarding(_)) {
            let hit = self
                .onboarding_hits
                .iter()
                .find(|(area, _)| area.contains(ratatui::layout::Position::new(x, y)))
                .map(|(_, hit)| *hit);
            if let Some(OnboardingHit::RailStep(step)) = hit
                && let Screen::Onboarding(wizard) = &mut self.screen
            {
                // The dialog is modal, like the settings': clicks
                // behind it do nothing.
                if wizard.dialog.is_none() {
                    wizard.go(step);
                }
            }
            return Vec::new();
        }
        if !matches!(self.screen, Screen::Settings(_)) {
            return Vec::new();
        }
        // The last-drawn hit wins: the add dialog draws over the page,
        // so its rows (and the flow's buttons) answer a click that
        // lands on both.
        let hit = self
            .settings_hits
            .iter()
            .rev()
            .find(|(area, _)| area.contains(ratatui::layout::Position::new(x, y)))
            .map(|(_, hit)| *hit);
        let Some(hit) = hit else {
            return Vec::new();
        };
        let action = match hit {
            SettingsHit::Sidebar(section) => {
                let Screen::Settings(settings) = &mut self.screen else {
                    return Vec::new();
                };
                if settings.modal_open() {
                    // A dialog is modal: clicks behind it do nothing.
                    return Vec::new();
                }
                let was = settings.section == crate::screens::SettingsSection::Models;
                settings.focus_section(section);
                let entered = !was && settings.section == crate::screens::SettingsSection::Models;
                if entered {
                    return self.models_page_opened();
                }
                return Vec::new();
            }
            SettingsHit::PageRow(index) => {
                let Screen::Settings(settings) = &mut self.screen else {
                    return Vec::new();
                };
                if settings.modal_open() {
                    // The whole set, not a list that drifts from the
                    // sidebar's.
                    return Vec::new();
                }
                settings.select_row(index);
                settings.enter_action()
            }
            SettingsHit::DialogCategory(index) => {
                let Screen::Settings(settings) = &mut self.screen else {
                    return Vec::new();
                };
                let Some(dialog) = &mut settings.dialog else {
                    return Vec::new();
                };
                dialog.select_category_row(index);
                dialog.choose_category();
                return Vec::new();
            }
            SettingsHit::DialogProvider(index) => {
                let Screen::Settings(settings) = &mut self.screen else {
                    return Vec::new();
                };
                let Some(dialog) = &mut settings.dialog else {
                    return Vec::new();
                };
                dialog.select_provider_row(index);
                return Vec::new();
            }
            SettingsHit::DialogApiChoice(index) => {
                let Screen::Settings(settings) = &mut self.screen else {
                    return Vec::new();
                };
                let Some(dialog) = &mut settings.dialog else {
                    return Vec::new();
                };
                dialog.select_api_row(index);
                return Vec::new();
            }
            SettingsHit::DialogFlowOpen => SettingsAction::OpenFlowUrl,
            SettingsHit::DialogFlowCopy => SettingsAction::CopyFlowUrl,
        };
        self.perform_settings(action)
    }

    /// Routes one keypress to the current screen, then the app-level
    /// handling (modals, quit, effects).
    pub fn on_key(&mut self, key: KeyEvent) -> Vec<Effect> {
        // A modifier pressed on its own is not a command: the kitty
        // protocol reports every key, so shift, control, and alt arrive
        // as events too. They must not end the selection, or the shift
        // a user presses before the copy key would erase the highlight
        // it is about to copy.
        if matches!(key.code, KeyCode::Modifier(_)) {
            return Vec::new();
        }
        // Ctrl-C copies the transcript selection, the terminal's copy
        // key. The selection stays, so the highlight does and a second
        // press copies again; there is no "nothing selected" fallback,
        // so a stray Ctrl-C is inert rather than fatal.
        if is_copy_key(key) {
            self.copy_selection();
            return Vec::new();
        }
        // Any other key ends the selection's life: the highlight is a
        // pointer artifact, not transcript content.
        self.selection = None;
        // Ctrl-D (EOF) quits immediately, from any screen, modal or
        // not. Ctrl-C is the copy key and never quits; on the chat
        // screen Esc opens the quit confirm.
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char('d') | KeyCode::Char('D'))
        {
            self.should_quit = true;
            return Vec::new();
        }
        // Ctrl-v on a dialog's text line reads the clipboard off the key
        // handler, the road the chat's paste key takes: the synchronous
        // read stalls the draw loop while the connection round trip runs,
        // and over ssh it can run for seconds.
        if key.code == KeyCode::Char('v')
            && key.modifiers.contains(KeyModifiers::CONTROL)
            && !key.modifiers.contains(KeyModifiers::ALT)
            && self.dialog_takes_paste()
        {
            return vec![Effect::GrabClipboard];
        }
        // The Models page asks for its catalog the moment it comes up:
        // the section is read before the key moves it, because the key
        // handler is what moves it.
        let mut entered_models = false;
        let routed = match &mut self.screen {
            Screen::Onboarding(wizard) => Routed::Onboarding(wizard.handle_key(key)),
            Screen::Picker(picker) => Routed::Picker(picker.handle_key(key)),
            Screen::Profile(profile) => Routed::Profile(profile.handle_key(key)),
            Screen::Resume(resume) => Routed::Resume(resume.handle_key(key)),
            Screen::Settings(settings) => {
                let was = settings.section == crate::screens::SettingsSection::Models;
                let action = settings.handle_key(key);
                entered_models =
                    !was && settings.section == crate::screens::SettingsSection::Models;
                Routed::Settings(action)
            }
            Screen::Chat => {
                let profiles = self.palette_profiles();
                let sandboxes = self.palette_sandboxes();
                Routed::Chat(self.chat.handle_key(
                    key,
                    &self.models,
                    &profiles,
                    &sandboxes,
                    self.icons,
                ))
            }
        };
        match routed {
            Routed::Onboarding(action) => self.perform_onboarding(action),
            Routed::Settings(action) => {
                // A page that comes up is a page that asks: leaving the
                // sidebar for Models loads the list, not only /model.
                let mut effects = self.perform_settings(action);
                if entered_models {
                    effects.extend(self.models_page_opened());
                }
                effects
            }
            // `/switch` is the only host of the full-screen picker: its
            // pick rides this session alone and never reaches the config.
            Routed::Picker(ModelPickerAction::Choose {
                provider,
                instance,
                id,
            }) => self.choose_session_model(&provider, instance.as_deref(), &id),
            Routed::Picker(ModelPickerAction::Back) => {
                self.screen = Screen::Chat;
                Vec::new()
            }
            Routed::Picker(ModelPickerAction::Retry) => self.retry_fetch(),
            Routed::Picker(ModelPickerAction::None) => Vec::new(),
            Routed::Profile(ProfileAction::Switch(name)) => self.switch_profile(&name),
            Routed::Profile(ProfileAction::BackToChat) => {
                self.screen = Screen::Chat;
                Vec::new()
            }
            Routed::Resume(crate::screens::ResumeAction::Pick(path)) => self.request_resume(&path),
            Routed::Resume(crate::screens::ResumeAction::ToggleAll) => self.toggle_resume_scope(),
            Routed::Resume(crate::screens::ResumeAction::BackToChat) => {
                self.screen = Screen::Chat;
                Vec::new()
            }
            Routed::Profile(ProfileAction::None)
            | Routed::Resume(crate::screens::ResumeAction::None) => Vec::new(),
            Routed::Chat(chat_action) => self.on_chat_key(key, chat_action),
        }
    }

    /// Performs one settings action, from a keypress or a click.
    pub(crate) fn perform_settings(&mut self, action: SettingsAction) -> Vec<Effect> {
        match action {
            SettingsAction::None => Vec::new(),
            SettingsAction::Back => {
                // The settings only ever open over a session: Esc means
                // "back to it".
                self.screen = Screen::Chat;
                Vec::new()
            }
            SettingsAction::ActivateInstance => self.activate_instance(),
            SettingsAction::RemoveInstance => self.remove_instance(),
            SettingsAction::SelectProfile => {
                let name = match &self.screen {
                    Screen::Settings(settings) => settings.selected_profile().map(str::to_owned),
                    _ => None,
                };
                match name {
                    Some(name) => self.switch_profile(&name),
                    None => Vec::new(),
                }
            }
            SettingsAction::RemoveProfile => self.remove_profile(),
            SettingsAction::CycleProfileColor => self.cycle_profile_color(),
            SettingsAction::OpenProfileDialog => {
                if let Screen::Settings(settings) = &mut self.screen {
                    settings.add_profile =
                        Some(crate::screens::AddProfileDialog::new(&self.config.profiles));
                }
                Vec::new()
            }
            SettingsAction::SubmitProfileDialog => self.create_profile(),
            SettingsAction::CloseProfileDialog => Vec::new(),
            SettingsAction::OpenDialog => {
                let Screen::Settings(settings) = &mut self.screen else {
                    return Vec::new();
                };
                settings.dialog = Some(AddInstanceDialog::new(&settings.stored));
                Vec::new()
            }
            SettingsAction::Validate => self.validate_login(),
            SettingsAction::StartFlow => vec![Effect::StartLoginFlow],
            SettingsAction::OpenFlowUrl => {
                self.open_active_flow_url();
                Vec::new()
            }
            SettingsAction::CopyFlowUrl => {
                self.copy_active_flow_url();
                Vec::new()
            }
            SettingsAction::CloseDialog => {
                // The dialog is already gone from the screen; dropping
                // the handle ends the flow task (the watch receiver
                // errors out), what Esc and a completed flow both want.
                self.login_cancel = None;
                Vec::new()
            }
            SettingsAction::ChooseModel => self.choose_picked_model(),
            SettingsAction::RetryModels => {
                if let Screen::Settings(settings) = &mut self.screen {
                    settings.model_picker.start_loading();
                }
                vec![Effect::FetchModels(FetchWhy::Refresh)]
            }
            SettingsAction::CyclePalette => {
                self.cycle_palette();
                Vec::new()
            }
            SettingsAction::ToggleTheme => {
                self.toggle_theme();
                Vec::new()
            }
            SettingsAction::ToggleBackground => {
                self.toggle_background();
                Vec::new()
            }
            SettingsAction::ToggleIcons => {
                self.toggle_icons();
                Vec::new()
            }
            SettingsAction::CycleCursor => vec![self.cycle_cursor_style()],
            SettingsAction::ToggleScrollbar => {
                self.toggle_scrollbar();
                Vec::new()
            }
            SettingsAction::SelectSandbox(choice) => self.choose_sandbox(choice),
            SettingsAction::OpenSandboxDialog => {
                if let Screen::Settings(settings) = &mut self.screen {
                    settings.sandbox_dialog = Some(crate::screens::AddSandboxDialog::new());
                }
                Vec::new()
            }
            SettingsAction::SubmitSandboxDialog => self.submit_sandbox_instance(),
            SettingsAction::CloseSandboxDialog => Vec::new(),
            SettingsAction::OpenMcpDialog => {
                self.open_mcp_dialog();
                Vec::new()
            }
            SettingsAction::SubmitMcpDialog => self.submit_mcp_server(),
            SettingsAction::CloseMcpDialog => Vec::new(),
            SettingsAction::ToggleMcpServer => {
                let name = match &self.screen {
                    Screen::Settings(settings) => settings.selected_mcp_server().map(str::to_owned),
                    _ => None,
                };
                match name {
                    Some(name) => {
                        self.toggle_mcp_server(&name);
                        Vec::new()
                    }
                    None => Vec::new(),
                }
            }
            SettingsAction::RemoveMcpServer => self.remove_mcp_server(),
            SettingsAction::ToggleWebSearch => {
                self.set_websearch_enabled(!self.effective().is_websearch_enabled());
                Vec::new()
            }
            SettingsAction::OpenWebSearchDialog => {
                self.open_websearch_dialog();
                Vec::new()
            }
            SettingsAction::SubmitWebSearchDialog => self.submit_websearch_provider(),
            SettingsAction::CloseWebSearchDialog => Vec::new(),
            SettingsAction::RemoveWebSearchProvider => self.remove_websearch_provider(),
            SettingsAction::MoveWebSearchUp => {
                self.move_websearch_provider(true);
                Vec::new()
            }
            SettingsAction::MoveWebSearchDown => {
                self.move_websearch_provider(false);
                Vec::new()
            }
            SettingsAction::ToggleDecision => {
                self.set_decision_enabled(!self.effective().is_decision_enabled());
                Vec::new()
            }
            SettingsAction::OpenDecisionDialog => {
                self.open_decision_dialog();
                Vec::new()
            }
            SettingsAction::SubmitDecisionDialog => self.submit_decision_provider(),
            SettingsAction::CloseDecisionDialog => Vec::new(),
            SettingsAction::RemoveDecisionProvider => self.remove_decision_provider(),
            SettingsAction::SetDefaultDecision => {
                self.set_default_decision();
                Vec::new()
            }
            SettingsAction::SelectSandboxInstance => {
                let name = match &self.screen {
                    Screen::Settings(settings) => {
                        settings.selected_sandbox_instance().map(str::to_owned)
                    }
                    _ => None,
                };
                match name {
                    Some(name) => self.activate_sandbox_instance(&name),
                    None => Vec::new(),
                }
            }
            SettingsAction::RemoveSandboxInstance => self.remove_sandbox_instance(),
            SettingsAction::InstallUpdate => {
                // Already out: a second Enter does not stack installers.
                if self.update.install == crate::screens::UpdateInstall::Running {
                    return Vec::new();
                }
                self.update.install = crate::screens::UpdateInstall::Running;
                self.sync_update_page();
                vec![Effect::InstallUpdate]
            }
            SettingsAction::EditSubagent => self.edit_subagent(),
            SettingsAction::ToggleSubagent => {
                let name = match &self.screen {
                    Screen::Settings(settings) => {
                        settings.selected_subagent().map(|row| row.name.clone())
                    }
                    _ => None,
                };
                match name {
                    Some(name) => {
                        self.toggle_subagent(&name);
                        Vec::new()
                    }
                    None => Vec::new(),
                }
            }
            SettingsAction::ToggleSkill => {
                let name = match &self.screen {
                    Screen::Settings(settings) => {
                        settings.selected_skill().map(|skill| skill.name.clone())
                    }
                    _ => None,
                };
                match name {
                    Some(name) => {
                        self.toggle_skill(&name);
                        Vec::new()
                    }
                    None => Vec::new(),
                }
            }
        }
    }

    /// Performs one onboarding action: the wizard's side effects, all
    /// of which touch the store, the session, or the network.
    /// Opens the running login flow's link in the default browser and
    /// notes the outcome under it. The browser launch is best effort:
    /// a box with no launcher gets the copy hint instead.
    fn open_active_flow_url(&mut self) {
        let Some(url) = self.active_flow_url() else {
            return;
        };
        let opened = crate::open_url::browser(&url);
        self.note_active_flow(if opened {
            "opening the browser"
        } else {
            "no browser launcher found; press c to copy the link"
        });
    }

    /// Copies the running login flow's link and notes it under the
    /// flow. The clipboard is best effort, the same write the
    /// transcript selection uses.
    fn copy_active_flow_url(&mut self) {
        let Some(url) = self.active_flow_url() else {
            return;
        };
        crate::screens::clipboard::write(&url);
        self.note_active_flow("link copied to the clipboard");
    }

    /// The link the open add dialog holds, from the settings or the
    /// wizard's login step.
    fn active_flow_url(&self) -> Option<String> {
        let dialog = match &self.screen {
            Screen::Settings(settings) => settings.dialog.as_ref(),
            Screen::Onboarding(wizard) => wizard.dialog.as_ref(),
            _ => None,
        }?;
        dialog.flow_url().map(str::to_owned)
    }

    /// Sets the one-line notice under the running flow.
    fn note_active_flow(&mut self, note: &str) {
        let dialog = match &mut self.screen {
            Screen::Settings(settings) => settings.dialog.as_mut(),
            Screen::Onboarding(wizard) => wizard.dialog.as_mut(),
            _ => None,
        };
        if let Some(dialog) = dialog {
            dialog.note_flow(note);
        }
    }

    pub(crate) fn perform_onboarding(&mut self, action: OnboardingAction) -> Vec<Effect> {
        match action {
            OnboardingAction::None => Vec::new(),
            OnboardingAction::Validate => self.validate_login(),
            OnboardingAction::StartFlow => vec![Effect::StartLoginFlow],
            OnboardingAction::OpenFlowUrl => {
                self.open_active_flow_url();
                Vec::new()
            }
            OnboardingAction::CopyFlowUrl => {
                self.copy_active_flow_url();
                Vec::new()
            }
            OnboardingAction::CancelFlow => {
                // The dialog is already gone from the screen; dropping
                // the handle ends the flow task.
                self.login_cancel = None;
                Vec::new()
            }
            OnboardingAction::ChooseModel {
                provider,
                instance,
                id,
            } => match self.apply_model_choice(&provider, instance.as_deref(), &id) {
                Ok(effects) => {
                    if let Screen::Onboarding(wizard) = &mut self.screen {
                        wizard.chosen_model = Some(id);
                        wizard.chosen_provider = Some(provider.to_owned());
                        wizard.go(crate::onboarding::OnboardingStep::Appearance);
                    }
                    effects
                }
                Err(err) => {
                    if let Screen::Onboarding(wizard) = &mut self.screen {
                        wizard.error = Some(err);
                    }
                    Vec::new()
                }
            },
            OnboardingAction::ToggleSkill(name) => {
                self.toggle_skill(&name);
                Vec::new()
            }
            OnboardingAction::SetPalette(palette) => {
                self.set_palette(palette);
                Vec::new()
            }
            OnboardingAction::SelectProfile(name) => self.select_onboarding_profile(&name),
            OnboardingAction::CreateProfile(name) => self.create_onboarding_profile(&name),
            OnboardingAction::Complete => {
                self.config.onboarded = Some(true);
                self.save_config();
                self.screen = Screen::Chat;
                self.transcript
                    .push_notice("setup complete, /settings any time; rhema onboard runs it again");
                Vec::new()
            }
            OnboardingAction::Quit => {
                self.should_quit = true;
                Vec::new()
            }
        }
    }

    /// Chat keys that are the app's, not the input box's.
    pub(crate) fn on_chat_key(&mut self, key: KeyEvent, action: ChatAction) -> Vec<Effect> {
        // Esc during a running turn cancels the turn, from anywhere,
        // approval modal included: the pending answer is released, the
        // rest declined, and the task drops the turn at its next await.
        // The y/N modal keys keep answering single calls.
        if self.turn.running && key.code == KeyCode::Esc {
            self.cancel_turn();
            return Vec::new();
        }
        // The approval modal takes every key while it is up.
        if self.turn.pending_approval.is_some() {
            match key.code {
                KeyCode::Char('y' | 'Y') => self.turn.answer(true),
                KeyCode::Char('n' | 'N') => self.turn.answer(false),
                _ => {}
            }
            return Vec::new();
        }
        if self.provider_followup.is_some() {
            match key.code {
                KeyCode::Char('y' | 'Y') => self.answer_provider_followup(true),
                KeyCode::Char('n' | 'N') | KeyCode::Esc => self.answer_provider_followup(false),
                _ => {}
            }
            return Vec::new();
        }
        if self.confirm_quit {
            match key.code {
                KeyCode::Char('y' | 'Y') => self.should_quit = true,
                KeyCode::Char('n' | 'N') | KeyCode::Esc => self.confirm_quit = false,
                _ => {}
            }
            return Vec::new();
        }
        // Plain Ctrl combos only: with Alt also held the key is AltGr
        // composing a character, which the input already took.
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && !key.modifiers.contains(KeyModifiers::ALT)
        {
            // Other Ctrl combos (including Ctrl-P mid-turn) are inert.
            if key.code == KeyCode::Char('p') && !self.turn.running {
                return self.open_model_settings();
            }
            // Ctrl-V only asks; the read runs off this handler and
            // lands later as an event, so its effect is one this gate
            // must pass through.
            if matches!(action, ChatAction::Paste) {
                return vec![Effect::GrabClipboard];
            }
            return Vec::new();
        }
        // Esc while entering a command cancels the entry instead of the
        // chat-level meanings (quit confirm / decline).
        if key.code == KeyCode::Esc
            && self.chat.input.starts_with('/')
            && !self.chat.input.contains('\n')
        {
            self.chat.input.clear();
            self.chat.cursor = 0;
            return Vec::new();
        }
        if key.code == KeyCode::Esc {
            if !self.turn.running {
                self.confirm_quit = true;
            }
            return Vec::new();
        }
        match action {
            ChatAction::Submit {
                text,
                images,
                spans,
            } => {
                // An image alone is a prompt: only a truly empty submit
                // is dropped.
                if text.trim().is_empty() && images.is_empty() {
                    Vec::new()
                } else if self.turn.running && !images.is_empty() {
                    // Steering stays text: an image rides a fresh prompt,
                    // and nothing here may silently strip one from a
                    // nudge. Everything stays for the next submit.
                    self.transcript
                        .push_notice("an image rides a fresh prompt; esc ends the turn first");
                    Vec::new()
                } else if self.turn.running {
                    // The box stays live while the turn runs: the text
                    // steers. `/settings` opens the snapshot screens over
                    // the chat instead, the turn keeps working in the
                    // background, and their instance actions still wait
                    // for idle on their own. Every other command stays
                    // idle-only, it acts on app state the running turn
                    // also touches, and keeps the input, as before.
                    if !text.contains('\n') && text.split_whitespace().next() == Some("/settings") {
                        self.chat.input.clear();
                        self.chat.cursor = 0;
                        self.chat.chips.clear();
                        crate::commands::dispatch(self, text.trim())
                    } else if !text.contains('\n') && text.trim().starts_with('/') {
                        Vec::new()
                    } else {
                        self.steer_turn(text)
                    }
                } else if images.is_empty() && !text.contains('\n') && text.trim().starts_with('/')
                {
                    self.chat.input.clear();
                    self.chat.cursor = 0;
                    self.chat.chips.clear();
                    // Commands read plain text: a paste embedded in the
                    // line expands for them, as it would for the model. A
                    // submission carrying images is a prompt, never a
                    // command, a command is a sentence, not a screenshot.
                    crate::commands::dispatch(self, text.trim())
                } else if self.provider.is_none() {
                    // The text stays: it is the prompt for once the
                    // profile has its key.
                    self.transcript
                        .push_notice("this profile has no API key; /login to add one");
                    Vec::new()
                } else if self.effective().model.is_none() {
                    // Same shape as the keyless case: the text stays for
                    // the model pick. The footer says "no model" in red
                    // all the while.
                    self.transcript
                        .push_notice("no model on this profile; /model to pick one");
                    Vec::new()
                } else if self.assembling.is_some() {
                    // The agent's tools are mid-rebuild: the text stays
                    // for the prompt to run once the assembly lands.
                    self.transcript
                        .push_notice("the sandbox is being stood up; the prompt waits for it");
                    Vec::new()
                } else if self.agent.is_none() {
                    // The step right after onboarding lands here: the
                    // profile is configured but the agent is not built
                    // yet. Starting a turn with no agent would leave
                    // `turn.running` set with no task behind it, spinner
                    // on and Esc dead; the text stays in the input and
                    // runs once the backend lands.
                    self.transcript
                        .push_notice("the backend is not ready yet; the prompt waits for it");
                    Vec::new()
                } else {
                    self.chat.input.clear();
                    self.chat.cursor = 0;
                    self.chat.chips.clear();
                    self.transcript
                        .push_user_with(&text, images.clone(), spans.clone());
                    // Queued nudges keep their transcript order ahead of
                    // a text-only line: the new line waits its turn
                    // behind them. A line carrying images runs now,
                    // images ride the first turn only, and the chain
                    // follows it through this turn's finish.
                    let text = if images.is_empty() && !self.pending_prompts.is_empty() {
                        self.pending_prompts.push_back(text);
                        self.pending_prompts
                            .pop_front()
                            .expect("the queue was just filled")
                    } else {
                        text
                    };
                    self.turn.running = true;
                    self.turn.started_at = Some(std::time::Instant::now());
                    self.error = None;
                    self.retryable = false;
                    self.turn.decline_all = false;
                    vec![Effect::StartTurn {
                        text,
                        images,
                        spans,
                    }]
                }
            }
            ChatAction::Notice { text } => {
                self.push_notice(&text);
                Vec::new()
            }
            ChatAction::Paste => vec![Effect::GrabClipboard],
            ChatAction::None => Vec::new(),
        }
    }

    /// Steers the running turn: the text echoes into the transcript now
    /// and rides the steer channel to the turn task, which drops the
    /// in-flight step at its current await and runs the text as the next
    /// turn on the same agent. A send can only fail once the task has
    /// exited, the finishing race, and the receiver it returned holds
    /// the message buffered for the turn-end handler to run.
    /// A submit while a turn runs: the text queues on the steer
    /// channel, the task drops the in-flight step at its current await
    /// and runs the text as the next turn on the same agent. A full
    /// queue refuses the nudge with the line back in the input; a send
    /// past task exit loses the race and the text waits in the input
    /// the same way.
    pub(crate) fn steer_turn(&mut self, text: String) -> Vec<Effect> {
        // A full queue refuses the nudge: the line stays in the input
        // exactly as it was typed, cursor and all, so nothing is lost
        // and nothing invisible queues up. The expanded form never
        // goes back: `text` has the paste payloads spliced over their
        // anchors, and writing it in would strand the payloads in
        // `chips` with no anchor left to carry them.
        if let Some(steer) = &self.turn.steer
            && steer.try_send(text.clone()).is_err()
        {
            return Vec::new();
        }
        self.chat.input.clear();
        self.chat.cursor = 0;
        self.chat.chips.clear();
        // A nudge carries no images: the steer channel is text-only.
        self.transcript.push_user(&text, Vec::new());
        vec![Effect::SteerTurn(text)]
    }

    /// `/switch` opens the full-screen picker over the cached models,
    /// fetching first when the cache is empty: the pick rides this
    /// session alone. The persisted pick is the settings' Models page.
    pub fn open_picker(&mut self) -> Vec<Effect> {
        // No candidate, no list to show: the runner would silently drop
        // the fetch and the picker would spin on Loading forever.
        if Backend::candidates_from_profile(&self.effective(), self.secret_store()).is_empty() {
            self.transcript
                .push_notice("no provider on this profile; /login first");
            return Vec::new();
        }
        let mut effects = Vec::new();
        // A provider added since the last fetch must show up even when
        // the cache still holds the other providers' rows.
        let loading = self.models.is_empty() || self.catalog_stale;
        if loading {
            effects.push(Effect::FetchModels(FetchWhy::Refresh));
            self.catalog_stale = false;
        }
        let mut picker = PickerScreen::new(self.models.clone());
        picker.unavailable = self.catalog_unavailable.clone();
        picker.failed = self.catalog_failed.clone();
        if loading {
            picker.start_loading();
        }
        self.screen = Screen::Picker(Box::new(picker));
        effects
    }
}
