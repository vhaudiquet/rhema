//! Session state: resume, clear, export, the checkpoint the exit
//! save rides, and the gauges and palette snapshots read from it.

use super::*;

impl App {
    /// Drops the conversation: a fresh transcript and a fresh session.
    /// The session file drops with it — what is typed after /clear is a
    /// new conversation, not a continuation of the loaded file, so the
    /// next turn allocates a file of its own instead of overwriting the
    /// cleared one's history.
    pub fn clear_session(&mut self) {
        if let Some(agent) = self.agent.as_mut() {
            agent.session_mut().history.clear();
        }
        self.transcript = Transcript::new();
        self.usage = None;
        self.session_cost = 0.0;
        self.error = None;
        self.retryable = false;
        self.session_file = None;
        // The benchmark describes one conversation: cleared with it,
        // in the fallback snapshot and in the agent's mirror alike.
        self.transcript_stats = None;
        if let Some(agent) = self.agent.as_mut() {
            agent.reset_transcript_stats();
        }
        self.transcript.push_notice("conversation cleared");
    }

    /// The file a turn checkpoints into and the exit save updates:
    /// `--save`'s explicit path wins; otherwise this conversation's own
    /// file. `None` when neither exists — the exit save then falls back
    /// to allocating at exit, and a turn simply cannot checkpoint.
    pub fn checkpoint_target(&self) -> Option<PathBuf> {
        self.options
            .save
            .clone()
            .or_else(|| self.session_file.clone())
    }

    /// The session to save for /resume, and the file it belongs to: the
    /// live session when its history holds anything, writing back to the
    /// origin when this session was resumed. A session of notices alone
    /// (a switch, a decline) has nothing resumable and saves nothing.
    pub fn export_session(&self) -> Option<(Session, Option<PathBuf>)> {
        let mut session = match &self.agent {
            Some(agent) => agent.session().clone(),
            None => self.session.clone(),
        };
        // The accounting rides with the save: a resumed session's
        // footer continues where this one ended, and its Usage page
        // keeps this run's numbers.
        session.cost_usd = self.session_cost;
        crate::sessions::UsageStats {
            transcript: self
                .agent
                .as_ref()
                .and_then(CodingAgent::transcript_stats)
                .or(self.transcript_stats),
            // The compaction rides with the counters: the resumed
            // session adopts the sections instead of re-summarizing.
            transcript_sections: self
                .agent
                .as_ref()
                .map(CodingAgent::transcript_sections)
                .filter(|sections| !sections.is_empty()),
            last_turn: self.usage,
        }
        .write_into(&mut session);
        if session.history.is_empty() {
            None
        } else {
            Some((session, self.session_file.clone()))
        }
    }

    /// Opens the resume picker over the sessions saved for this
    /// workspace. An empty history of saves is a notice, not a screen.
    pub fn open_resume(&mut self) -> Vec<Effect> {
        match crate::sessions::base_dir() {
            Ok(base) => self.open_resume_from(&base),
            Err(err) => {
                self.push_notice(&format!("cannot locate the session cache: {err}"));
                Vec::new()
            }
        }
    }

    /// [`App::open_resume`] against an explicit base, so tests drive real
    /// directories instead of the environment.
    pub fn open_resume_from(&mut self, base: &Path) -> Vec<Effect> {
        if self.turn.running {
            self.transcript
                .push_notice("a turn is running; /resume waits for it to finish");
            return Vec::new();
        }
        let workspace = std::fs::canonicalize(&self.options.workspace)
            .unwrap_or_else(|_| self.options.workspace.clone());
        match crate::sessions::list_sessions(base, &workspace) {
            Ok(entries) if entries.is_empty() => {
                self.transcript
                    .push_notice("no saved sessions for this directory");
            }
            Ok(entries) => {
                self.screen = Screen::Resume(crate::screens::ResumeScreen::new(
                    entries,
                    crate::sessions::now_millis(),
                ));
            }
            Err(err) => self.error = Some(format!("cannot list saved sessions: {err}")),
        }
        Vec::new()
    }

    /// Loads a saved session: rebuilds the agent on it, re-derives the
    /// transcript from its history, and lands in the chat. The current
    /// conversation is replaced — it keeps its own saved file from its
    /// own exit.
    pub fn resume_session(&mut self, path: &Path) {
        if self.turn.running {
            self.transcript
                .push_notice("a turn is running; /resume waits for it to finish");
            return;
        }
        let session = match CodingAgent::load_session_from(&path.to_path_buf()) {
            Ok(session) => session,
            Err(err) => {
                self.error = Some(format!("cannot load session: {err}"));
                return;
            }
        };
        let (mut agent, subagents) = match self.build_agent(session.clone()) {
            Ok(built) => built,
            Err(err) => {
                self.error = Some(err.to_string());
                return;
            }
        };
        let messages = session.history.len();
        self.session = session;
        // The saved usage accounting comes with it: the last turn's
        // tokens go straight to the gauge, and the transcript's
        // counters fold into the fresh mirror, so /settings's Usage
        // page keeps reading as one run.
        let stats = crate::sessions::UsageStats::from_session(&self.session);
        self.usage = stats.as_ref().and_then(|stats| stats.last_turn);
        if let Some(saved) = stats.as_ref().and_then(|stats| stats.transcript.as_ref()) {
            agent.carry_transcript_stats(saved);
        }
        if let Some(sections) = stats
            .as_ref()
            .and_then(|stats| stats.transcript_sections.clone())
        {
            agent.adopt_transcript_sections(sections);
        }
        self.transcript_stats = None;
        self.agent = Some(agent);
        self.subagents = Some(subagents);
        self.session_file = Some(path.to_path_buf());
        self.transcript = Transcript::from_history(&self.session.history);
        // The saved session carries what it spent; the footer continues
        // the story instead of quoting a fresh zero.
        self.session_cost = self.session.cost_usd;
        self.retryable = false;
        self.chat.scroll = 0;
        self.screen = Screen::Chat;
        self.transcript
            .push_notice(&format!("resumed {messages} messages"));
    }

    /// The settings that apply to this session's profile: field-wise, the
    /// named profile (or the one chosen at startup) over the top-level
    /// section. Computed on demand — it changes mid-session.
    pub fn effective(&self) -> rhema_config::Profile {
        self.config.effective(self.session_profile.as_deref())
    }

    /// The keychain this session resolves stored keys through, as the
    /// resolution tier under the environment; `None` reads literals only.
    pub fn secret_store(&self) -> Option<&dyn rhema_config::secrets::SecretStore> {
        self.secrets.as_deref()
    }

    /// The profiles the input palette completes `/profile` with: the
    /// config registry's rows, the session's own marked.
    pub fn palette_profiles(&self) -> Vec<crate::screens::PaletteProfile> {
        self.config
            .profiles
            .iter()
            .map(|(name, profile)| crate::screens::PaletteProfile {
                name: name.clone(),
                provider: profile.provider.clone(),
                model: profile.model.clone(),
                active: self.session_profile.as_deref() == Some(name.as_str()),
            })
            .collect()
    }

    /// The sandbox instances the input palette completes `/sandbox`
    /// with: the session profile's configured instances, the one the
    /// session currently runs `bash` in marked.
    pub fn palette_sandboxes(&self) -> Vec<crate::screens::PaletteSandbox> {
        let effective = self.config.effective(self.session_profile.as_deref());
        let active_name = effective
            .sandbox
            .as_ref()
            .and_then(|sandbox| sandbox.active.clone());
        let running = self.active_sandbox();
        effective
            .sandbox_instances
            .iter()
            .map(|(name, instance)| crate::screens::PaletteSandbox {
                name: name.clone(),
                kind: instance.kind.clone(),
                target: instance
                    .target
                    .clone()
                    .map(|target| target.trim().to_owned())
                    .filter(|target| !target.is_empty()),
                active: active_name.as_deref() == Some(name.as_str())
                    && SandboxChoice::from_stored(Some(&instance.kind)) == Some(running),
            })
            .collect()
    }

    /// The active profile's name and display color: the `--profile` flag
    /// over the stored active profile, colored by its config (a `#RRGGBB`
    /// string) or by a name-based theme tone.
    pub fn active_profile(&self) -> Option<(String, ratatui::style::Color)> {
        let name = self.session_profile.clone()?;
        let custom = self
            .config
            .profiles
            .get(&name)
            .and_then(|profile| profile.color.as_deref())
            .and_then(crate::palette::parse_hex_color);
        let color =
            custom.unwrap_or_else(|| crate::palette::default_profile_color(&name, &self.theme));
        Some((name, color))
    }

    /// The pricing of the model the session currently runs on, when its
    /// adapter prices it.
    pub(crate) fn current_model_pricing(&self) -> Option<rhema_provider::Pricing> {
        let effective = self.effective();
        let model = effective.model.as_deref()?;
        let active = self.backend.as_ref().map(Backend::config_name);
        self.models
            .iter()
            .find(|entry| entry.model.id == model && Some(entry.provider) == active)
            .and_then(|entry| entry.model.pricing)
    }

    /// The background subagent jobs' live state, read off the wire's
    /// registry handle: one entry per working job — queued or running.
    /// A finished job left already; its outcome arrived as a finish
    /// event. Empty without an agent or a wire.
    pub(crate) fn subagent_jobs(&self) -> Vec<rhema_subagents::JobStatus> {
        self.subagents
            .as_ref()
            .map(|handle| handle.status())
            .unwrap_or_default()
    }

    /// The pricing of any catalog model by id — the running subagent's
    /// model may differ from the session's, and its cost prices by its
    /// own card. An unknown id (the catalog never fetched it) prices to
    /// nothing.
    pub(crate) fn model_pricing(&self, model: &str) -> Option<rhema_provider::Pricing> {
        self.models
            .iter()
            .find(|entry| entry.model.id == model)
            .and_then(|entry| entry.model.pricing)
    }

    /// The context window of any catalog model by id — the running
    /// subagent's gauge is sized by its own card. `None` when unknown.
    pub(crate) fn model_window(&self, model: &str) -> Option<u64> {
        self.models
            .iter()
            .find(|entry| entry.model.id == model)
            .and_then(|entry| entry.model.input_token_limit)
    }

    /// The context window of the current model, when known.
    pub fn current_context_window(&self) -> Option<u64> {
        let effective = self.effective();
        let model = effective.model.as_deref()?;
        let active = self.backend.as_ref().map(Backend::config_name);
        self.models
            .iter()
            .find(|entry| entry.model.id == model && Some(entry.provider) == active)
            .and_then(|entry| entry.model.input_token_limit)
    }

    /// The context footprint the footer shows: the latest reported
    /// tally — the last turn's final usage, or while a turn streams the
    /// provider's running count, whose prompt already includes its reply
    /// — otherwise an estimate over the loaded history, which already
    /// includes every message. A resumed session has spent context
    /// before its first turn here, and an empty one estimates to zero.
    pub fn context_used(&self) -> u64 {
        match &self.usage {
            Some(usage) => context_tokens(usage),
            None => estimate_history_tokens(&self.live_session().history),
        }
    }

    /// The session the conversation currently lives in: the agent's when
    /// one is built, the bare one otherwise.
    pub(crate) fn live_session(&self) -> &Session {
        match &self.agent {
            Some(agent) => agent.session(),
            None => &self.session,
        }
    }
}
