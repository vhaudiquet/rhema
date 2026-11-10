//! Input handling: keys, clicks, paste, the chat line, and the
//! steering of a running turn. The router the screens feed lives
//! beside the handlers it calls.

use super::*;

/// The most bytes one paste may carry. A clipboard can hold anything;
/// the input is a prompt, not a file drop target.
const MAX_PASTE_BYTES: usize = 1024 * 1024;

impl App {
    /// In-band decline for a slash input no table row matches.
    pub fn unknown_command(&mut self, name: &str) {
        self.transcript
            .push_notice(&format!("unknown command {name}; /help lists commands"));
    }

    /// A notice into the transcript, styled by the transcript itself.
    pub fn push_notice(&mut self, text: &str) {
        self.transcript.push_notice(text);
    }

    /// A bracketed paste landed: into the input, newlines included —
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
        if !matches!(self.screen, Screen::Chat)
            || self.turn.pending_approval.is_some()
            || self.confirm_quit
        {
            return;
        }
        if self.transcript_view.starts_at(x, y) {
            self.selection = Some(super::selection::Selection::new((x, y)));
        }
    }

    /// The drag moved: the selection follows the pointer, clamped into
    /// the transcript so a drag past an edge still selects the text it
    /// crossed.
    pub fn on_mouse_drag(&mut self, x: u16, y: u16) {
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
        if self.selection.is_none() {
            return false;
        }
        self.on_mouse_drag(x, y);
        let Some(selection) = self.selection else {
            return true;
        };
        let text = super::selection::text(&self.transcript_view, selection);
        if !text.is_empty() {
            crate::screens::clipboard::write(&text);
        }
        true
    }

    /// A left click landed. The settings menu answers clicks — sidebar
    /// rows switch sections, page rows select-and-act, the add dialog's
    /// provider rows pick — everywhere else the keyboard rules. The hit
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
        let hit = self
            .settings_hits
            .iter()
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
                if settings.dialog.is_some()
                    || settings.add_profile.is_some()
                    || settings.mcp_dialog.is_some()
                    || settings.mcp_confirm.is_some()
                {
                    // A dialog is modal: clicks behind it do nothing.
                    return Vec::new();
                }
                settings.section = section;
                settings.row = 0;
                return Vec::new();
            }
            SettingsHit::PageRow(index) => {
                let Screen::Settings(settings) = &mut self.screen else {
                    return Vec::new();
                };
                if settings.dialog.is_some()
                    || settings.add_profile.is_some()
                    || settings.sandbox_dialog.is_some()
                    || settings.mcp_dialog.is_some()
                    || settings.mcp_confirm.is_some()
                {
                    return Vec::new();
                }
                settings.row = index;
                settings.enter_action()
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
        };
        self.perform_settings(action)
    }

    /// Routes one keypress to the current screen, then the app-level
    /// handling (modals, quit, effects).
    pub fn on_key(&mut self, key: KeyEvent) -> Vec<Effect> {
        // Any key ends the selection's life: the highlight is a pointer
        // artifact, not transcript content.
        self.selection = None;
        // Ctrl-C and Ctrl-D (EOF) quit immediately, from any screen,
        // modal or not.
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(
                key.code,
                KeyCode::Char('c') | KeyCode::Char('C') | KeyCode::Char('d') | KeyCode::Char('D')
            )
        {
            self.should_quit = true;
            return Vec::new();
        }
        let routed = match &mut self.screen {
            Screen::Onboarding(wizard) => Routed::Onboarding(wizard.handle_key(key)),
            Screen::Picker(picker) => Routed::Picker(picker.handle_key(key)),
            Screen::Profile(profile) => Routed::Profile(profile.handle_key(key)),
            Screen::Resume(resume) => Routed::Resume(resume.handle_key(key)),
            Screen::Settings(settings) => Routed::Settings(settings.handle_key(key)),
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
            Routed::Settings(action) => self.perform_settings(action),
            Routed::Picker(PickerAction::Choose {
                provider,
                instance,
                id,
            }) => self.choose_model(provider, instance.as_deref(), &id),
            Routed::Picker(PickerAction::BackToChat) => {
                self.screen = Screen::Chat;
                Vec::new()
            }
            Routed::Profile(ProfileAction::Switch(name)) => self.switch_profile(&name),
            Routed::Profile(ProfileAction::BackToChat) => {
                self.screen = Screen::Chat;
                Vec::new()
            }
            Routed::Resume(crate::screens::ResumeAction::Pick(path)) => self.request_resume(&path),
            Routed::Resume(crate::screens::ResumeAction::BackToChat) => {
                self.screen = Screen::Chat;
                Vec::new()
            }
            Routed::Picker(PickerAction::Retry) => self.retry_fetch(),
            Routed::Picker(PickerAction::None)
            | Routed::Profile(ProfileAction::None)
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
            SettingsAction::CloseDialog => {
                // The dialog is already gone from the screen; dropping
                // the handle ends the flow task (the watch receiver
                // errors out) — what Esc and a completed flow both want.
                self.login_cancel = None;
                Vec::new()
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
            SettingsAction::CheckForUpdates => {
                self.update.check = crate::screens::UpdateCheck::Checking;
                self.sync_update_page();
                vec![Effect::CheckForUpdates]
            }
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
    pub(crate) fn perform_onboarding(&mut self, action: OnboardingAction) -> Vec<Effect> {
        match action {
            OnboardingAction::None => Vec::new(),
            OnboardingAction::Validate => self.validate_login(),
            OnboardingAction::StartFlow => vec![Effect::StartLoginFlow],
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
                self.transcript.push_notice(
                    "setup complete — /settings any time; rhema onboard runs it again",
                );
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
        // Esc during a running turn cancels the turn, from anywhere —
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
                return self.open_picker();
            }
            return Vec::new();
        }
        // Esc while entering a command cancels the entry instead of the
        // chat-level meanings (quit confirm / decline).
        if key.code == KeyCode::Esc && self.chat.input.starts_with('/') {
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
            ChatAction::Submit { text, images } => {
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
                    // the chat instead — the turn keeps working in the
                    // background, and their instance actions still wait
                    // for idle on their own. Every other command stays
                    // idle-only — it acts on app state the running turn
                    // also touches — and keeps the input, as before.
                    if text.split_whitespace().next() == Some("/settings") {
                        self.chat.input.clear();
                        self.chat.cursor = 0;
                        self.chat.chips.clear();
                        crate::commands::dispatch(self, rhema_core::paste::expand(&text).trim())
                    } else if text.trim().starts_with('/') {
                        Vec::new()
                    } else {
                        self.steer_turn(text)
                    }
                } else if images.is_empty() && text.trim().starts_with('/') {
                    self.chat.input.clear();
                    self.chat.cursor = 0;
                    self.chat.chips.clear();
                    // Commands read plain text: a paste embedded in the
                    // line expands for them, as it would for the model. A
                    // submission carrying images is a prompt, never a
                    // command — a command is a sentence, not a screenshot.
                    crate::commands::dispatch(self, rhema_core::paste::expand(&text).trim())
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
                } else {
                    self.chat.input.clear();
                    self.chat.cursor = 0;
                    self.chat.chips.clear();
                    self.transcript.push_user(&text, images.clone());
                    // Queued nudges keep their transcript order ahead of
                    // a text-only line: the new line waits its turn
                    // behind them. A line carrying images runs now —
                    // images ride the first turn only — and the chain
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
                    vec![Effect::StartTurn { text, images }]
                }
            }
            ChatAction::None => Vec::new(),
        }
    }

    /// Steers the running turn: the text echoes into the transcript now
    /// and rides the steer channel to the turn task, which drops the
    /// in-flight step at its current await and runs the text as the next
    /// turn on the same agent. A send can only fail once the task has
    /// exited — the finishing race — and the receiver it returned holds
    /// the message buffered for the turn-end handler to run.
    /// A submit while a turn runs: the text queues on the steer
    /// channel, the task drops the in-flight step at its current await
    /// and runs the text as the next turn on the same agent. A full
    /// queue refuses the nudge with the line back in the input; a send
    /// past task exit loses the race and the text waits in the input
    /// the same way.
    pub(crate) fn steer_turn(&mut self, text: String) -> Vec<Effect> {
        // A full queue refuses the nudge: the line goes back into the
        // input, so nothing is lost and nothing invisible queues up.
        if let Some(steer) = &self.turn.steer
            && steer.try_send(text.clone()).is_err()
        {
            self.chat.input = text;
            self.chat.cursor = self.chat.input.chars().count();
            return Vec::new();
        }
        self.chat.input.clear();
        self.chat.cursor = 0;
        self.chat.chips.clear();
        // A nudge carries no images: the steer channel is text-only.
        self.transcript.push_user(&text, Vec::new());
        vec![Effect::SteerTurn(text)]
    }

    /// Ctrl-P mid-session: open the picker over the cached models,
    /// fetching first when the cache is empty.
    pub fn open_picker(&mut self) -> Vec<Effect> {
        // No backend, no list to show: the runner would silently drop
        // the fetch and the picker would spin on Loading forever.
        if self.backend.is_none() {
            self.transcript
                .push_notice("no provider on this profile; /login first");
            return Vec::new();
        }
        let mut effects = Vec::new();
        let mut status = PickerStatus::Ready;
        if self.models.is_empty() {
            status = PickerStatus::Loading;
            effects.push(Effect::FetchModels(FetchWhy::Refresh));
        }
        self.screen = Screen::Picker(PickerScreen {
            status,
            ..PickerScreen::new(self.models.clone())
        });
        effects
    }
}
