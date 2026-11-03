//! The application state machine: screens, transitions, and the routing of
//! keypresses and UI events. Pure logic — effects that touch the network
//! or spawn turns are returned to the runner (`crate::run`), which owns
//! the runtime and the terminal.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use rhema_coding_agent::{CodingAgent, CodingAgentConfig, CodingAgentError};
use rhema_config::Config;
use rhema_core::{
    AgentError, Content, Image, Message, ModelOutput, ModelProvider, Session, StopReason, ToolCall,
    Usage,
};
use rhema_provider::{Allowance, ModelCard};

use crate::approval::ApprovalAnswerer;
use crate::args::SandboxChoice;
use crate::events::{FetchWhy, OwnedEvent, UiEvent};
use crate::onboarding::{OnboardingAction, OnboardingScreen};
use crate::screens::{
    AddInstanceDialog, ChatAction, ChatState, PickerAction, PickerScreen, PickerStatus,
    ProfileAction, ProfileScreen, SettingsAction, SettingsScreen, SettingsSection, SkillRow,
    SkillSource, SubagentRow, SubagentSource,
};
use crate::store::ConfigStore;
use crate::transcript::Transcript;

mod setup;

pub use setup::{
    AppSetup, Backend, Candidate, Effect, OnboardingHit, RunOptions, Screen, SettingsHit,
};
pub(crate) use setup::{NO_PROVIDER_HELP, context_tokens, startup_profile, startup_sandbox};
use setup::{
    estimate_history_tokens, profile_sandbox, session_agent_config, shadow_builtin, skill_rows,
    subagent_rows,
};

/// The whole application state.
pub struct App {
    /// Config persistence seam.
    pub store: Arc<dyn ConfigStore>,
    /// The keychain logins place keys in; `None` keeps them in the file.
    pub secrets: Option<Arc<dyn rhema_config::secrets::SecretStore>>,
    /// The config as currently held; mutations persist through `store`.
    pub config: Config,
    /// The backend resolved at login (or startup), if any.
    pub backend: Option<Backend>,
    /// Steered nudges that raced a turn's finish and wait their turn:
    /// one `StartTurn` leaves per turn finish (the dispatch takes the
    /// agent once), so the rest chain through each finish. The
    /// transcript showed every one as a sent user line at submit time;
    /// this queue is the promise they reach the model in that order.
    pending_prompts: std::collections::VecDeque<String>,
    /// The provider behind the backend.
    pub provider: Option<Arc<dyn ModelProvider>>,
    /// The startup flags.
    pub options: RunOptions,
    /// The home directory, when the environment names one: the footer
    /// trims it (and its `Documents`) off the workspace path it shows.
    pub home: Option<PathBuf>,
    /// The machine-global config root the settings pages discover
    /// under: the setup's home when one is given, else the
    /// environment's own resolution.
    config_root: Option<PathBuf>,
    /// The active backend's subscription allowance, when one was
    /// fetched: the backend's config name rides along, so a report of a
    /// backend the session has since left never shows. A failed fetch
    /// clears the badge — a stale figure reads as truth.
    pub subscription: Option<(&'static str, Option<Allowance>)>,
    /// The profile this session runs under: the `--profile` flag over the
    /// stored active profile. `/profile` switches it mid-session.
    pub session_profile: Option<String>,
    /// The starting session, used when the first agent is built.
    pub session: Session,
    /// The current screen.
    pub screen: Screen,
    /// Cached model catalog, shared with the picker when it opens: every
    /// logged-in provider's models, each tagged with its provider.
    pub models: Vec<crate::screens::CatalogModel>,
    /// The agent; `None` while a turn runs (the turn task owns it).
    pub agent: Option<CodingAgent>,
    /// The subagent job registry the current agent's tool resolver
    /// starts jobs into — the wire's handle, replaced at every agent
    /// rebuild, exactly when the old registry dies with its agent. The
    /// status strip renders one live line per job from it.
    pub subagents: Option<rhema_subagents::SubagentHandle>,
    /// The turn lifecycle: what the keyboard, the renderer, and the
    /// status tracker need while the agent is out with a task.
    pub turn: TurnState,
    /// The chat transcript.
    pub transcript: Transcript,
    /// Token accounting of the last finished turn, kept current during
    /// one by the provider's live tally: the gauge moves as the turn
    /// streams, not only when it ends.
    pub usage: Option<Usage>,
    /// The compaction benchmark from the last turn that ran with an
    /// agent present: request counts, flushes, and the prompt-cache
    /// hit rate the providers reported.
    pub transcript_stats: Option<rhema_transcript::TranscriptStats>,
    /// Accumulated cost of the session in USD, summed per turn from the
    /// current model's pricing; turns on unpriced models add nothing.
    pub session_cost: f64,
    /// The last turn's error, shown in the status line until the next
    /// submit.
    pub error: Option<String>,
    /// Whether the failed turn can be re-run: the last finished turn
    /// errored and nothing has run since. A submit, a cancel, a success,
    /// and a conversation change (`/clear`, `/resume`) clear it — the
    /// request `/retry` re-issues must be the last thing this
    /// conversation did. An agent rebuild keeps it on purpose: a model,
    /// instance, or profile switch carries the same conversation, and
    /// "switch then retry" is half the point.
    pub retryable: bool,
    /// The quit confirmation overlay is up.
    pub confirm_quit: bool,
    /// The cancel handle of the running login flow; `None` while idle.
    /// Dropping it ends the flow task — the watch receiver errors out —
    /// exactly what Esc and a completed flow both want.
    pub login_cancel: Option<tokio::sync::watch::Sender<bool>>,
    /// The file this conversation persists to: `--resume`'s own file, a
    /// resumed-through-/resume file, or the fresh name the runner
    /// allocated. The exit save updates it in place instead of growing a
    /// second entry, and every turn checkpoints into it as its streams
    /// land. `None` once /clear drops the conversation — the next turn
    /// allocates a new file for what follows.
    pub session_file: Option<PathBuf>,
    /// Set by the runner to end the event loop.
    pub should_quit: bool,
    /// The resolved design tokens every render pass reads.
    pub theme: crate::palette::Theme,
    /// The resolved footer glyphs every render pass reads.
    pub icons: &'static crate::icons::Icons,
    /// The chat input state.
    pub chat: ChatState,
    /// The clickable rects of the settings screen, re-registered by
    /// every frame's render and matched against click positions.
    pub settings_hits: Vec<(ratatui::layout::Rect, SettingsHit)>,
    /// The same for the onboarding wizard's rail.
    pub onboarding_hits: Vec<(ratatui::layout::Rect, OnboardingHit)>,
    /// The transcript render's row cache: wrapped rows per block,
    /// invalided by width, theme, and the transcript's watermark.
    pub(crate) transcript_cache: crate::ui::WrapCache,
    /// The session's assembled sandbox, when `bash` runs on one:
    /// `None` is the local fast path — every agent build applies this
    /// when set, so a mid-session rebuild never drops it.
    pub sandboxed: Option<crate::sandbox::Sandboxed>,
    /// The machine half of the availability probe: the facts the
    /// offered set re-derives from at every profile switch, without
    /// paying the CLI probes again.
    pub sandbox_machines: crate::sandbox::SandboxMachines,
    /// What this machine offers the session's current profile: the
    /// picker's rows and the select path's availability gate. Derived
    /// from the machine facts at startup and re-derived at every
    /// profile switch — ssh and http answer on the profile's own
    /// section, not the machine's alone.
    pub sandbox_offered: crate::sandbox::SandboxAvailability,
    /// The sandbox assembly in flight, if any: the picker's choice is
    /// being stood up off the UI thread. Turns and further switches
    /// wait for it; the answer arrives as `UiEvent::SandboxAssembled`.
    pub assembling: Option<SandboxChoice>,
    /// Where the onboarding welcome step drew its logo, when it did:
    /// the runner places the terminal-graphics image there after the
    /// frame, and clears it when the rect goes away. `None` on every
    /// other screen.
    pub logo_rect: Option<ratatui::layout::Rect>,
}

/// How many steered nudges may sit queued between the turn's step
/// boundaries. The channel is bounded: nothing drains it mid-step, and
/// a held key under repeat fires submits faster than any step — a full
/// queue refuses the nudge instead of pinning an unbounded buffer.
pub(crate) const STEER_QUEUE_CAP: usize = 64;

/// The lifecycle state of the turn the UI thread handed to a task. The
/// agent comes back in a `TurnFinished` or `TurnCancelled` event; this is
/// what the keyboard, the renderer, and the status tracker need until
/// then. Between turns everything rests at its [`TurnState::idle`] default.
pub struct TurnState {
    /// Whether a turn is in flight.
    pub running: bool,
    /// When the turn started, for the elapsed-time indicator.
    pub started_at: Option<std::time::Instant>,
    /// The cancel flag of the running turn; the task watches the
    /// receiver. The runner creates the pair at submit.
    pub cancel: Option<tokio::sync::watch::Sender<bool>>,
    /// The steering channel of the running turn: a submit while the turn
    /// runs lands here, and the task interrupts the in-flight step with
    /// it. The task returns the receiver in its final event, so a message
    /// sent in the finishing race sits buffered there instead of
    /// vanishing. `None` while no turn runs.
    steer: Option<tokio::sync::mpsc::Sender<String>>,
    /// The tool call currently awaiting the user's answer.
    pub pending_approval: Option<ToolCall>,
    /// Every future approval request is declined without a modal (the
    /// turn is being cancelled; nothing more may ask).
    pub decline_all: bool,
    /// The approval answer channel; `None` while no turn runs, making
    /// any late answer a no-op.
    answers: Option<ApprovalAnswerer>,
    /// A command is executing while the turn runs; `false` while the
    /// model itself is streaming — thinking.
    pub processing: bool,
    /// The running turn's reasoning trace, tail only: the status strip
    /// previews its last rows, and it clears the moment real output
    /// starts. Never part of the transcript or the session history.
    pub thinking: String,
    /// The running command's output, tail only: the status strip
    /// previews its completed lines while the command runs, and the
    /// whole text lands with the tool result. Never part of the
    /// transcript or the session history.
    pub tool_output: String,
    /// When the running command's first output chunk landed: the
    /// preview stays hidden until the command has earned it — a second
    /// of runtime or a second line.
    pub tool_output_at: Option<std::time::Instant>,
    /// The provider retry wait in flight, if one was announced: a
    /// transient failure happened and the ladder is counting down to
    /// the next attempt. Any further event ends it — the next attempt
    /// is speaking — and `finished` clears it with the rest.
    pub retry_wait: Option<TurnRetryWait>,
}

/// A provider retry wait the status strip counts down: one announced
/// wait, carried from the engine's event to the next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnRetryWait {
    /// The failed attempt's error text.
    pub error: String,
    /// When the next attempt fires; the strip counts down to it.
    pub deadline: std::time::Instant,
    /// One-based number of the attempt that just failed.
    pub attempt: u32,
    /// Total attempts the policy will spend.
    pub total: u32,
}

impl TurnState {
    /// The state between turns.
    fn idle() -> Self {
        Self {
            running: false,
            started_at: None,
            cancel: None,
            steer: None,
            pending_approval: None,
            decline_all: false,
            answers: None,
            processing: false,
            thinking: String::new(),
            tool_output: String::new(),
            tool_output_at: None,
            retry_wait: None,
        }
    }

    /// A turn was accepted: the clock starts in the thinking phase, the
    /// approval answer channel is wired, and the previous turn's residue
    /// (a stale modal, a preview buffer) is gone.
    fn running(
        cancel: tokio::sync::watch::Sender<bool>,
        steer: tokio::sync::mpsc::Sender<String>,
        answers: Option<ApprovalAnswerer>,
    ) -> Self {
        Self {
            running: true,
            started_at: Some(std::time::Instant::now()),
            cancel: Some(cancel),
            steer: Some(steer),
            answers,
            ..Self::idle()
        }
    }

    /// The turn is over, finished or cancelled: one reset for both, so
    /// they cannot drift.
    fn finished(&mut self) {
        self.running = false;
        self.started_at = None;
        self.cancel = None;
        self.steer = None;
        self.processing = false;
        self.thinking.clear();
        self.retry_wait = None;
        self.decline_all = false;
        self.pending_approval = None;
        self.answers = None;
    }

    /// Answers the pending approval and clears the modal; without a
    /// listening turn task, a no-op.
    fn answer(&mut self, approved: bool) {
        if let Some(answers) = &self.answers {
            answers.answer(approved);
        }
        self.pending_approval = None;
    }

    /// Appends one reasoning piece to the preview buffer, keeping only
    /// the tail: the status strip shows at most a few wrapped rows, so
    /// older reasoning has no screen to live on.
    fn push_thinking(&mut self, delta: &str) {
        const TAIL_CAP: usize = 2048;
        self.thinking.push_str(delta);
        if self.thinking.len() > TAIL_CAP {
            let mut cut = self.thinking.len() - TAIL_CAP;
            while !self.thinking.is_char_boundary(cut) {
                cut += 1;
            }
            self.thinking.drain(..cut);
        }
    }

    /// Appends one output chunk of the running command to the tool
    /// preview buffer, tail-capped like the reasoning buffer; the first
    /// chunk starts the reveal clock.
    fn push_tool_output(&mut self, chunk: &str) {
        const TAIL_CAP: usize = 2048;
        if self.tool_output_at.is_none() {
            self.tool_output_at = Some(std::time::Instant::now());
        }
        self.tool_output.push_str(chunk);
        if self.tool_output.len() > TAIL_CAP {
            let mut cut = self.tool_output.len() - TAIL_CAP;
            while !self.tool_output.is_char_boundary(cut) {
                cut += 1;
            }
            self.tool_output.drain(..cut);
        }
    }

    /// The running command's preview goes: its result landed, or the
    /// next call started.
    fn clear_tool_output(&mut self) {
        self.tool_output.clear();
        self.tool_output_at = None;
    }
}

/// One keypress routed to the active screen.
enum Routed {
    Onboarding(OnboardingAction),
    Picker(PickerAction),
    Profile(ProfileAction),
    Resume(crate::screens::ResumeAction),
    Settings(SettingsAction),
    Chat(ChatAction),
}

impl App {
    /// Builds the app and its initial screen. Errors only when a stored
    /// model should open the chat but the agent cannot be built.
    ///
    /// The first screen is the onboarding wizard when the runner says
    /// this is a first run (`setup.onboard`) — and the chat in every
    /// other case, including a config with no usable backend or no
    /// stored model: the footer names what is missing in red, the
    /// commands to fix it are one keystroke away, and nothing gates the
    /// session behind a login screen.
    pub fn new(setup: AppSetup) -> Result<Self, CodingAgentError> {
        let session = setup.session;
        let transcript = Transcript::from_history(&session.history);
        // The settings that apply to this session's profile are computed
        // on demand (they change when the model switches or a login lands).
        let mut agent = None;
        let mut subagents_handle = None;
        // Startup reads straight off the config: App::effective exists on
        // the struct, which does not exist yet.
        let effective = setup.config.effective(setup.options.profile.as_deref());
        let config_root = match setup.home.clone() {
            Some(home) => Some(rhema_config::Config::dir_under(&home)),
            None => rhema_config::Config::dir().ok(),
        };
        let screen = if setup.onboard {
            let workspace = std::fs::canonicalize(&setup.options.workspace)
                .unwrap_or_else(|_| setup.options.workspace.clone());
            let agents = setup
                .home
                .as_deref()
                .map(rhema_config::Config::agents_dir_under);
            let skills = skill_rows(
                agents.as_deref(),
                config_root
                    .as_deref()
                    .map(|root| root.join("skills"))
                    .as_deref(),
                &workspace,
                &setup.config.skills_disabled,
            );
            Screen::Onboarding(Box::new(OnboardingScreen::new(
                skills,
                setup
                    .options
                    .profile
                    .clone()
                    .unwrap_or_else(|| rhema_config::DEFAULT_PROFILE.to_owned()),
                setup.backend.is_some(),
                effective.clone(),
            )))
        } else {
            match (&setup.backend, &effective.model) {
                (Some(_), Some(_)) => {
                    let provider = setup
                        .backend
                        .as_ref()
                        .map(Backend::provider)
                        .ok_or(AgentError::MissingProvider)?;
                    let (agent_config, subagents) = session_agent_config(
                        &setup.options,
                        setup.sandboxed.as_ref(),
                        effective.model.clone(),
                        provider.clone(),
                        &setup.config.skills_disabled,
                    );
                    agent = Some(CodingAgent::from_session(
                        provider,
                        agent_config,
                        session.clone(),
                    )?);
                    subagents_handle = Some(subagents);
                    Screen::Chat
                }
                // No model yet — or no usable backend at all — still
                // opens the chat: the footer says "no model" in red and
                // the submit box names the fix, /model or /login.
                _ => Screen::Chat,
            }
        };
        Ok(Self {
            secrets: setup.secrets,
            // Read before `options` moves into the struct.
            theme: setup
                .options
                .colors
                .theme(setup.options.theme, setup.options.palette_inputs()),
            icons: setup.options.icons.set(),
            store: setup.store,
            config: setup.config,
            provider: setup.backend.as_ref().map(Backend::provider),
            backend: setup.backend,
            pending_prompts: std::collections::VecDeque::new(),
            session_profile: Some(
                setup
                    .options
                    .profile
                    .clone()
                    .unwrap_or_else(|| rhema_config::DEFAULT_PROFILE.to_owned()),
            ),
            options: setup.options,
            home: setup
                .home
                .clone()
                .or_else(|| std::env::var("HOME").ok().map(PathBuf::from)),
            config_root,
            subscription: None,
            session,
            screen,
            models: Vec::new(),
            agent,
            subagents: subagents_handle,
            turn: TurnState::idle(),
            transcript,
            usage: None,
            transcript_stats: None,
            session_cost: 0.0,
            error: None,
            retryable: false,
            confirm_quit: false,
            login_cancel: None,
            session_file: setup.checkpoint,
            should_quit: false,
            chat: ChatState::new(),
            settings_hits: Vec::new(),
            onboarding_hits: Vec::new(),
            transcript_cache: crate::ui::WrapCache::new(),
            sandboxed: setup.sandboxed,
            sandbox_machines: setup.sandbox_machines,
            sandbox_offered: setup.sandbox_machines.offered_for(&effective),
            assembling: None,
            logo_rect: None,
        })
    }

    /// Effects the runner must perform once, before the first draw.
    pub fn initial_effects(&self) -> Vec<Effect> {
        if self.backend.is_some() {
            vec![Effect::FetchModels(FetchWhy::Bootstrap), Effect::FetchUsage]
        } else {
            Vec::new()
        }
    }

    /// Handles one keypress; returns the effects to perform.
    /// Switches the session to a profile: re-resolves the backend from
    /// that profile's settings, rebuilds the agent carrying the session
    /// over, persists the choice, and notices. A profile without usable
    /// credentials is declined in-band.
    /// Opens the profile picker over the config's profiles.
    pub fn open_profile_picker(&mut self) -> Vec<Effect> {
        if self.config.profiles.is_empty() {
            self.push_notice("no profiles defined in the config");
            return Vec::new();
        }
        self.screen = Screen::Profile(crate::screens::ProfileScreen::new(
            &self.config.profiles,
            self.session_profile.as_deref(),
            self.secret_store(),
        ));
        Vec::new()
    }

    /// The kind this session's `bash` currently runs under: the
    /// assembled sandbox's choice, `local` when the agent's own tools
    /// run.
    pub fn active_sandbox(&self) -> SandboxChoice {
        self.sandboxed
            .as_ref()
            .map(|assembled| assembled.choice)
            .unwrap_or(SandboxChoice::Local)
    }

    /// The footer's sandbox badge: the kind `bash` runs under, or the
    /// instance's name when the profile's active pointer names the
    /// instance this kind resolves to. Local reads as a fact too — the
    /// fast path is a choice, not an absence.
    pub fn sandbox_badge(&self) -> String {
        let choice = self.active_sandbox();
        if choice == SandboxChoice::Local {
            return "local".to_owned();
        }
        self.effective()
            .sandbox_instance_for(choice.stored_name())
            .map(|(name, _)| name.to_owned())
            .unwrap_or_else(|| choice.stored_name().to_owned())
    }

    /// `/sandbox <name>`: a configured instance's name runs `bash` in
    /// it — the Sandbox page's enter, which points the section's active
    /// instance at the name; anything else parses as a kind, the
    /// picker's select with the kind spelled out. An unknown name is
    /// declined in-band.
    pub fn choose_sandbox_by_name(&mut self, name: &str) -> Vec<Effect> {
        if self
            .config
            .effective(self.session_profile.as_deref())
            .sandbox_instances
            .contains_key(name)
        {
            return self.activate_sandbox_instance(name);
        }
        match SandboxChoice::from_stored(Some(name)) {
            Some(choice) => self.choose_sandbox(choice),
            None => {
                self.transcript.push_notice(&format!(
                    "no sandbox {name:?}; /sandbox lists this profile's instances and the kinds"
                ));
                Vec::new()
            }
        }
    }

    /// Switches where the session's `bash` runs: the kind persists on
    /// the session's profile's `sandbox` section (the kind only — the
    /// url, host, container, and token keep whatever the config stored),
    /// the workspace remembers it, and the running agent's tools are
    /// rebuilt on the shared assembly. `local` rebuilds inline; every
    /// other kind assembles off the UI thread and answers as
    /// `UiEvent::SandboxAssembled` — until then the current tools keep
    /// running. A kind this machine does not offer is declined in-band.
    pub fn choose_sandbox(&mut self, choice: SandboxChoice) -> Vec<Effect> {
        if self.turn.running {
            self.transcript
                .push_notice("a turn is running; /sandbox waits for it to finish");
            return Vec::new();
        }
        if self.assembling.is_some() {
            self.transcript
                .push_notice("a sandbox is being stood up; /sandbox waits for it");
            return Vec::new();
        }
        if !self.sandbox_offered.offers(choice) {
            self.transcript.push_notice(&format!(
                "{} is not offered on this machine; /sandbox lists what is",
                choice.stored_name()
            ));
            return Vec::new();
        }
        if self.active_sandbox() == choice {
            self.transcript
                .push_notice(&format!("bash already runs on {}", choice.stored_name()));
            return Vec::new();
        }
        // Kind only: the rest of the section is the config's to keep —
        // the picker does not edit urls, hosts, or tokens.
        self.config
            .set_for_profile(self.session_profile.as_deref(), |profile| {
                let sandbox = profile
                    .sandbox
                    .get_or_insert_with(rhema_config::SandboxSettings::default);
                sandbox.kind = Some(choice.stored_name().to_owned());
            });
        // The workspace remembers the kind, the twin of the profile
        // memory `begin_turn` writes.
        self.config
            .note_sandbox_in(&self.options.workspace, choice.stored_name());
        if let Err(err) = self.store.save(&self.config) {
            self.error = Some(format!("cannot save config: {err}"));
            return Vec::new();
        }
        // Enter selects and returns: the picker closes on an accepted
        // choice, the assembly's answer arrives over the chat.
        self.screen = Screen::Chat;
        match choice {
            SandboxChoice::Local => {
                self.replace_sandboxed(None);
                self.rebuild_for_sandbox("bash runs on this host");
                Vec::new()
            }
            non_local => {
                self.assembling = Some(non_local);
                self.transcript.push_notice(&format!(
                    "standing up the {} sandbox…",
                    non_local.stored_name()
                ));
                vec![Effect::AssembleSandbox(non_local)]
            }
        }
    }

    /// Swaps the session's assembled sandbox, stopping the outgoing
    /// one first: a switch to another instance — or back to local — is
    /// an exit for the outgoing session's hold, and its stop-on-exit
    /// answer belongs to this moment, not to the process's end.
    fn replace_sandboxed(&mut self, assembled: Option<crate::sandbox::Sandboxed>) {
        if let Some(outgoing) = self.sandboxed.as_mut() {
            outgoing.shutdown();
        }
        self.sandboxed = assembled;
    }

    /// Rebuilds the running agent carrying the conversation over — the
    /// profile-switch idiom — so the session's sandbox state takes
    /// effect on the tool's executor at once, then notices.
    fn rebuild_for_sandbox(&mut self, notice: &str) {
        // The machine changed: the report re-states where bash runs.
        if let Err(err) = self.rebuild_live_agent(true) {
            self.error = Some(err);
            return;
        }
        self.transcript.push_notice(notice);
    }

    /// In-band decline for a slash input no table row matches.
    pub fn unknown_command(&mut self, name: &str) {
        self.transcript
            .push_notice(&format!("unknown command {name}; /help lists commands"));
    }

    /// A notice into the transcript, styled by the transcript itself.
    pub fn push_notice(&mut self, text: &str) {
        self.transcript.push_notice(text);
    }

    /// Opens the settings screen on `section`. `/settings` and `/login`
    /// both land on Providers — the login flow lives there now. The
    /// Providers page edits the session's profile live; the other pages
    /// read snapshots of this moment. Openable mid-turn: only the
    /// instance actions wait for an idle session.
    pub fn open_settings_at(&mut self, section: SettingsSection) {
        let mut settings = Self::build_settings_screen(
            &self.config,
            &self.options,
            self.session_profile.as_deref(),
            self.home.as_deref(),
            self.config_root.as_deref(),
            self.secret_store(),
        );
        settings.section = section;
        settings.allowance = self.subscription.clone();
        settings.last_turn = self.usage;
        settings.transcript = self
            .agent
            .as_ref()
            .and_then(CodingAgent::transcript_stats)
            .or(self.transcript_stats);
        settings.session_cost = self.session_cost;
        settings.sandbox_offered = self.sandbox_offered;
        settings.sandbox_active = self.active_sandbox();
        settings.sandbox_remembered = self
            .config
            .last_sandbox_in(&self.options.workspace)
            .as_deref()
            .and_then(|kind| SandboxChoice::from_stored(Some(kind)));
        self.screen = Screen::Settings(Box::new(settings));
    }

    /// Assembles the settings screen: the live Providers page over the
    /// session's profile, plus the other pages' snapshots.
    fn build_settings_screen(
        config: &Config,
        options: &RunOptions,
        session_profile: Option<&str>,
        home: Option<&Path>,
        config_root: Option<&Path>,
        secrets: Option<&dyn rhema_config::secrets::SecretStore>,
    ) -> SettingsScreen {
        let effective = config.effective(session_profile);
        let workspace =
            std::fs::canonicalize(&options.workspace).unwrap_or_else(|_| options.workspace.clone());
        let agents_dir = home.map(rhema_config::Config::agents_dir_under);
        let global_skills = config_root.map(|root| root.join("skills"));
        let skills = skill_rows(
            agents_dir.as_deref(),
            global_skills.as_deref(),
            &workspace,
            &config.skills_disabled,
        );
        // The same discovery the agent's wiring runs, so the page shows
        // what the next session will delegate to.
        let global_subagents = config_root.map(|root| root.to_path_buf());
        let mut settings = SettingsScreen::new();
        settings.colors = options.colors;
        settings.theme_dark = options.theme == crate::palette::ThemeKind::Dark;
        settings.icons_nerd = options.icons == crate::icons::Flavor::Nerd;
        settings.profile = session_profile
            .unwrap_or(rhema_config::DEFAULT_PROFILE)
            .to_owned();
        settings.stored = effective;
        settings.profiles =
            crate::screens::profile_rows(&config.profiles, session_profile, secrets);
        settings.skills = skills;
        settings.skills_global_dir = global_skills.map(|dir| dir.to_string_lossy().into_owned());
        settings.skills_workspace_dir = workspace
            .join(".rhema")
            .join("skills")
            .to_string_lossy()
            .into_owned();
        settings.subagents = subagent_rows(global_subagents.as_deref(), &workspace);
        settings.subagents_global_dir =
            global_subagents.map(|dir| dir.to_string_lossy().into_owned());
        settings.subagents_workspace_dir = workspace
            .join(".rhema")
            .join("subagents")
            .to_string_lossy()
            .into_owned();
        settings.config_path = rhema_config::Config::path()
            .ok()
            .map(|path| path.to_string_lossy().into_owned());
        settings.workspace = workspace.to_string_lossy().into_owned();
        settings.profile_count = config.profiles.len();
        settings
    }

    /// Re-reads the Subagents page's snapshot: an edit happened in
    /// `$EDITOR` behind the app's back, and the rows must show it when
    /// the TUI re-enters. A no-op when the settings screen is not open.
    pub fn refresh_subagents(&mut self) {
        let Screen::Settings(settings) = &mut self.screen else {
            return;
        };
        let workspace = std::fs::canonicalize(&self.options.workspace)
            .unwrap_or_else(|_| self.options.workspace.clone());
        let global = self.config_root.clone();
        settings.subagents = subagent_rows(global.as_deref(), &workspace);
        settings.subagents_global_dir = global.map(|dir| dir.to_string_lossy().into_owned());
        settings.subagents_workspace_dir = workspace
            .join(".rhema")
            .join("subagents")
            .to_string_lossy()
            .into_owned();
        if settings.row >= settings.subagents.len() {
            settings.row = 0;
        }
    }

    /// Opens the selected subagent definition in `$EDITOR`. The built-in
    /// has no file: the first edit copies it into the global directory,
    /// where a definition of the same name shadows the built-in from
    /// then on. The editor itself runs in the runner — the TUI must be
    /// suspended around it — and the rows refresh when it returns.
    fn edit_subagent(&mut self) -> Vec<Effect> {
        let Screen::Settings(settings) = &self.screen else {
            return Vec::new();
        };
        let Some(row) = settings.selected_subagent() else {
            return Vec::new();
        };
        if let Some(path) = &row.path {
            return vec![Effect::EditSubagent(path.clone())];
        }
        // Only the built-in row reaches here; its shadow lands in the
        // global directory discovery reads first.
        match rhema_config::Config::dir() {
            Ok(dir) => match shadow_builtin(&dir, &row.name) {
                Ok(path) => vec![Effect::EditSubagent(path)],
                Err(err) => {
                    self.transcript
                        .push_notice(&format!("cannot copy the built-in definition: {err}"));
                    Vec::new()
                }
            },
            Err(err) => {
                self.transcript.push_notice(&format!(
                    "no config directory to copy the built-in into: {err}"
                ));
                Vec::new()
            }
        }
    }

    /// Flips the appearance and persists the choice: the swap is live
    /// (the tokens change this frame), and the config records it for the
    /// next startup. The open settings page follows, so it keeps telling
    /// the truth.
    fn toggle_theme(&mut self) {
        let dark = self.options.theme == crate::palette::ThemeKind::Dark;
        let kind = if dark {
            crate::palette::ThemeKind::Light
        } else {
            crate::palette::ThemeKind::Dark
        };
        self.options.theme = kind;
        self.theme = self
            .options
            .colors
            .theme(kind, self.options.palette_inputs());
        self.config.theme = Some(if dark { "light" } else { "dark" }.to_owned());
        if let Screen::Settings(settings) = &mut self.screen {
            settings.theme_dark = !dark;
        }
        if let Err(err) = self.store.save(&self.config) {
            self.error = Some(format!("cannot save config: {err}"));
        }
    }

    /// Cycles to the next palette and persists the choice, live like the
    /// theme toggle. The open settings page follows.
    fn cycle_palette(&mut self) {
        let all = crate::palette::Palette::ALL;
        let current = all
            .iter()
            .position(|palette| *palette == self.options.colors)
            .unwrap_or(0);
        self.set_palette(all[(current + 1) % all.len()]);
    }

    /// Applies one palette live — the frame this call renders already
    /// wears it — and persists the choice. The wizard's Colors step and
    /// the settings' Appearance page both land here; the open screen
    /// follows, so the rows keep telling the truth.
    fn set_palette(&mut self, palette: crate::palette::Palette) {
        self.options.colors = palette;
        self.theme = palette.theme(self.options.theme, self.options.palette_inputs());
        self.config.colors = Some(palette.stored_name().to_owned());
        if let Screen::Settings(settings) = &mut self.screen {
            settings.colors = palette;
        }
        if let Err(err) = self.store.save(&self.config) {
            self.error = Some(format!("cannot save config: {err}"));
        }
    }

    /// Flips the footer's icon flavor and persists the choice, live like
    /// the theme toggle.
    fn toggle_icons(&mut self) {
        let nerd = self.options.icons != crate::icons::Flavor::Nerd;
        let flavor = if nerd {
            crate::icons::Flavor::Nerd
        } else {
            crate::icons::Flavor::Unicode
        };
        self.options.icons = flavor;
        self.icons = flavor.set();
        self.config.icons = Some(if nerd { "nerd" } else { "unicode" }.to_owned());
        if let Screen::Settings(settings) = &mut self.screen {
            settings.icons_nerd = nerd;
        }
        if let Err(err) = self.store.save(&self.config) {
            self.error = Some(format!("cannot save config: {err}"));
        }
    }

    /// Flips a skill's on/off state: the disabled set in the config
    /// flips, a bundled skill being switched back on is re-seeded when
    /// its folder is gone, and the running agent rebuilds carrying the
    /// conversation over — the system prompt carries the skills list,
    /// so the very next turn sees the change. The open screen's row
    /// follows (settings page or wizard step); a running turn defers
    /// the rebuild to its end, the toggle itself lands immediately.
    fn toggle_skill(&mut self, name: &str) {
        let turning_on = self.config.skills_disabled.contains(name);
        if turning_on {
            self.config.skills_disabled.remove(name);
        } else {
            self.config.skills_disabled.insert(name.to_owned());
        }
        // A bundled skill switched back on must exist on disk again —
        // seeding skips disabled names, so its folder may be gone.
        if turning_on && let Some(root) = self.config_root.clone() {
            let _ = crate::bundled::seed_one(&root.join("skills"), name);
        }
        if let Err(err) = self.store.save(&self.config) {
            self.error = Some(format!("cannot save config: {err}"));
            return;
        }
        // The open screen's row follows: the settings page or the
        // wizard step, whichever is showing.
        match &mut self.screen {
            Screen::Settings(settings) => {
                for row in &mut settings.skills {
                    if row.name == name {
                        row.enabled = !row.enabled;
                    }
                }
            }
            Screen::Onboarding(wizard) => {
                for row in &mut wizard.skills {
                    if row.name == name {
                        row.enabled = !row.enabled;
                    }
                }
            }
            _ => {}
        }
        // The prompt change rides the next agent build; mid-turn the
        // rebuild would race the running one, so it waits.
        if !self.turn.running
            && let Err(err) = self.rebuild_live_agent(false)
        {
            self.error = Some(err);
        }
    }

    /// The profile step's accepted rename: the config's profile entry
    /// moves to the new name (its settings ride along), the session
    /// follows, and the workspace's profile memory forgets the old
    /// name. A blank or unchanged name never reaches here.
    fn rename_session_profile(&mut self, new_name: &str) {
        let Some(old) = self.session_profile.clone() else {
            return;
        };
        if new_name == old || self.config.profiles.contains_key(new_name) {
            return;
        }
        let entry = self.config.profiles.remove(&old).unwrap_or_default();
        self.config.profiles.insert(new_name.to_owned(), entry);
        self.session_profile = Some(new_name.to_owned());
        self.config
            .last_profile_by_workspace
            .retain(|_, remembered| remembered != &old);
        if let Err(err) = self.store.save(&self.config) {
            self.error = Some(format!("cannot save config: {err}"));
        }
    }

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

    /// Switches the session to a profile: re-resolves the backend from
    /// that profile's settings, rebuilds the agent carrying the session
    /// over, and notices. The choice is per-session, never persisted. A
    /// profile without usable credentials still switches — the session's
    /// identity is what moves — but the old provider goes with the old
    /// profile: turns wait for /login rather than leaking the previous
    /// profile's key.
    pub fn switch_profile(&mut self, name: &str) -> Vec<Effect> {
        if self.turn.running {
            self.transcript
                .push_notice("a turn is running; /profile waits for it to finish");
            return Vec::new();
        }
        if self.assembling.is_some() {
            self.transcript
                .push_notice("a sandbox is being stood up; /profile waits for it");
            return Vec::new();
        }
        if self.session_profile.as_deref() == Some(name) {
            self.transcript
                .push_notice(&format!("already on profile {name}"));
            return Vec::new();
        }
        let effective = self.config.effective(Some(name));
        self.options.thinking = effective.thinking_effort();
        // Profiles are isolated, `bash` included: the offered set
        // re-derives for the profile moving in, and where its own
        // `sandbox` section says `bash` runs is where this session's
        // `bash` goes — local when the profile names nothing, never a
        // sandbox the profile just left behind.
        self.sandbox_offered = self.sandbox_machines.offered_for(&effective);
        let wanted = profile_sandbox(&effective, &self.sandbox_offered);
        let keep = match self.sandboxed.as_ref() {
            None => wanted == SandboxChoice::Local,
            Some(current) => {
                current.choice == wanted
                    && wanted != SandboxChoice::Local
                    && Some(current.target.as_str())
                        == crate::sandbox::sandbox_target(&effective, wanted.stored_name())
                            .as_deref()
            }
        };
        if !keep && wanted == SandboxChoice::Local {
            self.replace_sandboxed(None);
        }
        let session = match &self.agent {
            Some(agent) => agent.session().clone(),
            None => self.session.clone(),
        };
        let Some(backend) = Backend::from_profile(&effective, self.secret_store()) else {
            // Never recorded: startup re-validates usability, so the
            // memory naming a keyless profile would only fall through.
            // The agent drops with the provider: a turn here would run
            // on the profile just left. The sandbox leaves with the
            // profile too — nothing keyless can run a turn in it.
            if !keep {
                self.replace_sandboxed(None);
            }
            self.session_profile = Some(name.to_owned());
            self.backend = None;
            self.provider = None;
            self.agent = None;
            self.models.clear();
            self.session = session;
            self.screen = Screen::Chat;
            self.transcript.push_notice(&format!(
                "switched to profile {name}; no API key, /login to add one"
            ));
            return Vec::new();
        };
        let (agent_config, subagents) = session_agent_config(
            &self.options,
            self.sandboxed.as_ref(),
            effective.model.clone(),
            backend.provider(),
            &self.config.skills_disabled.clone(),
        );
        match CodingAgent::from_session(backend.provider(), agent_config, session) {
            Ok(mut agent) => {
                // The benchmark moves with the conversation, the way
                // the cost does: the old agent's counters fold into
                // the fresh mirror.
                let saved = self
                    .agent
                    .as_ref()
                    .and_then(CodingAgent::transcript_stats)
                    .or(self.transcript_stats);
                let sections = self
                    .agent
                    .as_ref()
                    .map(CodingAgent::transcript_sections)
                    .unwrap_or_default();
                if let Some(saved) = saved {
                    agent.carry_transcript_stats(&saved);
                }
                agent.adopt_transcript_sections(sections);
                self.session_profile = Some(name.to_owned());
                self.backend = Some(backend.clone());
                self.provider = Some(backend.provider());
                self.models.clear();
                self.agent = Some(agent);
                self.subagents = Some(subagents);
                self.screen = Screen::Chat;
                // An explicit switch is a choice: recorded for this
                // workspace now, not at the next turn.
                self.remember_profile();
                let mut notice = format!("switched to profile {name}");
                if effective.model.is_none() {
                    notice.push_str("; /model to pick a model");
                }
                let mut effects = vec![Effect::FetchUsage];
                if keep {
                    // The profile moving in resolves to the sandbox
                    // already running: nothing moves.
                } else if wanted == SandboxChoice::Local {
                    notice.push_str("; bash runs on this host");
                } else {
                    // The old tools keep running until the assembly
                    // lands (the picker's posture): the answer arrives
                    // as `UiEvent::SandboxAssembled`, which swaps the
                    // tools and rebuilds again.
                    self.assembling = Some(wanted);
                    notice.push_str(&format!(
                        "; standing up the {} sandbox…",
                        wanted.stored_name()
                    ));
                    effects.push(Effect::AssembleSandbox(wanted));
                }
                self.transcript.push_notice(&notice);
                effects
            }
            Err(err) => {
                self.error = Some(err.to_string());
                Vec::new()
            }
        }
    }

    /// Refills the settings Profiles page's snapshot from the config:
    /// the registry the add, remove, and color edits just changed.
    fn sync_profile_rows(&mut self) {
        let rows = crate::screens::profile_rows(
            &self.config.profiles,
            self.session_profile.as_deref(),
            self.secret_store(),
        );
        if let Screen::Settings(settings) = &mut self.screen {
            settings.profiles = rows;
        }
    }

    /// The add-profile dialog's accepted name: a fresh, empty profile
    /// joins the registry — provider, model, and key stay unset, the
    /// Providers page's login flow fills those in — and the snapshot
    /// follows. The session does not move: creation is not a switch.
    fn create_profile(&mut self) -> Vec<Effect> {
        let name = match &self.screen {
            Screen::Settings(settings) => match &settings.add_profile {
                Some(dialog) => dialog.name().to_owned(),
                None => return Vec::new(),
            },
            _ => return Vec::new(),
        };
        if name.is_empty() {
            return Vec::new();
        }
        if self.config.profiles.contains_key(&name) {
            // The dialog dedupes against its opening snapshot; this is
            // the belt for a path that should not exist.
            self.transcript
                .push_notice(&format!("a profile named {name} already exists"));
            return Vec::new();
        }
        self.config
            .profiles
            .insert(name.clone(), rhema_config::Profile::default());
        if let Err(err) = self.store.save(&self.config) {
            self.error = Some(format!("cannot save config: {err}"));
            return Vec::new();
        }
        if let Screen::Settings(settings) = &mut self.screen {
            settings.add_profile = None;
        }
        self.sync_profile_rows();
        self.transcript
            .push_notice(&format!("added profile {name}"));
        Vec::new()
    }

    /// The add-sandbox dialog's accepted kind and value: the kind's
    /// field lands on the session profile's `sandbox` section — the
    /// container, the host, or the url — and the snapshot follows.
    /// Storing never activates: enter on a kind row runs `bash` there.
    fn submit_sandbox_instance(&mut self) -> Vec<Effect> {
        let kind = match &self.screen {
            Screen::Settings(settings) => match &settings.sandbox_dialog {
                Some(dialog) => dialog.picked(),
                None => return Vec::new(),
            },
            _ => return Vec::new(),
        };
        // The instance carries no target: the concrete name is the
        // workspace's to set, derived at assembly time and stable
        // across sessions. The row's name is the kind's — first free,
        // `docker-2` once `docker` is taken. Storing never activates.
        let mut name = kind.stored_name().to_owned();
        let mut suffix = 2;
        while self
            .config
            .effective(self.session_profile.as_deref())
            .sandbox_instances
            .contains_key(&name)
        {
            name = format!("{}-{suffix}", kind.stored_name());
            suffix += 1;
        }
        self.config
            .set_for_profile(self.session_profile.as_deref(), |profile| {
                profile.place_sandbox_instance(
                    name.clone(),
                    rhema_config::SandboxInstance {
                        kind: kind.stored_name().to_owned(),
                        target: None,
                        vm: None,
                    },
                );
            });
        if let Err(err) = self.store.save(&self.config) {
            self.error = Some(format!("cannot save config: {err}"));
            return Vec::new();
        }
        if let Screen::Settings(settings) = &mut self.screen {
            settings.sandbox_dialog = None;
            settings.stored = self.config.effective(self.session_profile.as_deref());
        }
        self.transcript.push_notice(&format!(
            "the {name} instance is stored; enter its row to run bash there"
        ));
        Vec::new()
    }

    /// `d` on a Sandbox page instance row: remove the instance. The
    /// record goes from the map and the section's active pointer goes
    /// with it; a session already running there keeps running, the
    /// providers' stance. The next assembly under the stored kind
    /// falls through to the section's facts or a derived name.
    fn remove_sandbox_instance(&mut self) -> Vec<Effect> {
        let name = match &self.screen {
            Screen::Settings(settings) => settings.selected_sandbox_instance().map(str::to_owned),
            _ => return Vec::new(),
        };
        let Some(name) = name else {
            return Vec::new();
        };
        let mut removed = None;
        self.config
            .set_for_profile(self.session_profile.as_deref(), |profile| {
                removed = profile.remove_sandbox_instance(&name);
            });
        if removed.is_none() {
            return Vec::new();
        }
        if let Err(err) = self.store.save(&self.config) {
            self.error = Some(format!("cannot save config: {err}"));
            return Vec::new();
        }
        self.transcript
            .push_notice(&format!("the {name} sandbox instance is removed"));
        if let Screen::Settings(settings) = &mut self.screen {
            settings.stored = self.config.effective(self.session_profile.as_deref());
        }
        Vec::new()
    }

    /// Enter on a Sandbox page instance row: run `bash` in it — the
    /// same flow `/sandbox <kind>` runs, with the section's active
    /// pointer naming the instance so the assembly reads its target.
    /// The session's kind moves only when the instance's kind differs;
    /// two instances of one kind switch without touching the kind.
    fn activate_sandbox_instance(&mut self, name: &str) -> Vec<Effect> {
        let instance = self
            .config
            .effective(self.session_profile.as_deref())
            .sandbox_instances
            .get(name)
            .cloned();
        let Some(instance) = instance else {
            return Vec::new();
        };
        let Some(choice) = crate::args::SandboxChoice::from_stored(Some(&instance.kind)) else {
            self.transcript.push_notice(&format!(
                "the {name} instance's kind is not one rhema runs: {}",
                instance.kind
            ));
            return Vec::new();
        };
        if self.turn.running {
            self.transcript
                .push_notice("a turn is running; the sandbox waits for it to finish");
            return Vec::new();
        }
        if self.assembling.is_some() {
            self.transcript
                .push_notice("a sandbox is being stood up; the sandbox waits for it");
            return Vec::new();
        }
        if !self.sandbox_offered.offers(choice) {
            self.transcript.push_notice(&format!(
                "{} is not offered on this machine; the {name} instance cannot run here",
                choice.stored_name()
            ));
            return Vec::new();
        }
        let active_name = self
            .config
            .effective(self.session_profile.as_deref())
            .sandbox
            .as_ref()
            .and_then(|sandbox| sandbox.active.clone());
        if active_name.as_deref() == Some(name) && self.active_sandbox() == choice {
            self.transcript
                .push_notice(&format!("bash already runs in the {name} instance"));
            return Vec::new();
        }
        self.config
            .set_for_profile(self.session_profile.as_deref(), |profile| {
                let sandbox = profile
                    .sandbox
                    .get_or_insert_with(rhema_config::SandboxSettings::default);
                sandbox.kind = Some(choice.stored_name().to_owned());
                sandbox.active = Some(name.to_owned());
            });
        self.config
            .note_sandbox_in(&self.options.workspace, choice.stored_name());
        if let Err(err) = self.store.save(&self.config) {
            self.error = Some(format!("cannot save config: {err}"));
            return Vec::new();
        }
        // Enter selects and returns: the page closes on an accepted
        // choice, the assembly's answer arrives over the chat.
        self.screen = Screen::Chat;
        match choice {
            SandboxChoice::Local => {
                self.replace_sandboxed(None);
                self.rebuild_for_sandbox("bash runs on this host");
                Vec::new()
            }
            non_local => {
                self.assembling = Some(non_local);
                self.transcript
                    .push_notice(&format!("standing up the {name} sandbox instance…"));
                vec![Effect::AssembleSandbox(non_local)]
            }
        }
    }

    /// `d` on a Profiles page row: forget the profile. The one this
    /// session runs on stays — the session's identity is not dropped
    /// from under it, and that refusal is also what keeps the registry
    /// from emptying. The workspace memory forgets the name with the
    /// registry, the way a rename does.
    fn remove_profile(&mut self) -> Vec<Effect> {
        let Some(name) = (match &self.screen {
            Screen::Settings(settings) => settings.selected_profile().map(str::to_owned),
            _ => None,
        }) else {
            return Vec::new();
        };
        if self.session_profile.as_deref() == Some(name.as_str()) {
            self.transcript
                .push_notice("the profile this session runs on cannot be removed — switch first");
            return Vec::new();
        }
        self.config.profiles.remove(&name);
        self.config
            .last_profile_by_workspace
            .retain(|_, remembered| remembered != &name);
        if let Err(err) = self.store.save(&self.config) {
            self.error = Some(format!("cannot save config: {err}"));
            return Vec::new();
        }
        self.sync_profile_rows();
        // Keep the selection inside the shrunken list.
        if let Screen::Settings(settings) = &mut self.screen {
            let rows = settings.profiles.len() + 1;
            if settings.row >= rows {
                settings.row = rows.saturating_sub(1);
            }
        }
        self.transcript
            .push_notice(&format!("removed profile {name}"));
        Vec::new()
    }

    /// `c` on a Profiles page row: the name color moves to the next
    /// entry of the default wheel — the theme's auto first, then each
    /// default hex — and persists. The page's swatch is the record;
    /// no notice.
    fn cycle_profile_color(&mut self) -> Vec<Effect> {
        let Some(name) = (match &self.screen {
            Screen::Settings(settings) => settings.selected_profile().map(str::to_owned),
            _ => None,
        }) else {
            return Vec::new();
        };
        let stored = self
            .config
            .profiles
            .get(&name)
            .and_then(|profile| profile.color.as_deref());
        let next = Self::next_profile_color(stored);
        self.config.set_for_profile(Some(&name), |profile| {
            profile.color = next.map(str::to_owned)
        });
        if let Err(err) = self.store.save(&self.config) {
            self.error = Some(format!("cannot save config: {err}"));
            return Vec::new();
        }
        self.sync_profile_rows();
        Vec::new()
    }

    /// The next entry of the profile color wheel after `stored`: auto
    /// (no stored color) first, then each default hex, wrapping to
    /// auto. A hand-edited color the wheel does not spell rejoins at
    /// auto.
    fn next_profile_color(stored: Option<&str>) -> Option<&'static str> {
        match stored {
            None => Some(crate::palette::PROFILE_COLORS[0]),
            Some(hex) => crate::palette::PROFILE_COLORS
                .iter()
                .position(|color| color.eq_ignore_ascii_case(hex))
                .and_then(|index| crate::palette::PROFILE_COLORS.get(index + 1))
                .copied(),
        }
    }

    /// Sets the session's thinking effort: persisted into the session's
    /// profile like a model pick, and the agent rebuilds carrying the
    /// conversation so the very next turn asks at the new level.
    pub fn set_thinking(&mut self, effort: rhema_core::request::ThinkingEffort) {
        if self.turn.running {
            self.transcript
                .push_notice("a turn is running; /thinking waits for it to finish");
            return;
        }
        self.options.thinking = effort;
        self.config
            .set_for_profile(self.session_profile.as_deref(), |settings| {
                settings.thinking = Some(effort.as_str().to_owned());
            });
        if let Err(err) = self.store.save(&self.config) {
            self.error = Some(format!("cannot save config: {err}"));
            return;
        }
        if let Err(err) = self.rebuild_live_agent(false) {
            self.error = Some(err);
            return;
        }
        self.transcript
            .push_notice(&format!("thinking effort set to {}", effort.as_str()));
    }

    /// The system prompt in the transcript, fenced so it reads verbatim.
    pub fn show_system_prompt(&mut self, prompt: &str) {
        self.transcript
            .push_markdown(format!("```markdown\n{prompt}```"));
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

    /// A bracketed paste landed: into the input, newlines included —
    /// pasting multi-line text must not fire submits. The add-instance
    /// dialog's credential line takes its own, single-line shape.
    pub fn on_paste(&mut self, text: &str) {
        match &mut self.screen {
            Screen::Chat => self.chat.paste(text),
            Screen::Settings(settings) => {
                if let Some(dialog) = &mut settings.dialog {
                    dialog.paste(text);
                }
                if let Some(dialog) = &mut settings.add_profile {
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
    /// (one wheel tick), so both controls move in lockstep.
    pub fn on_mouse_scroll(&mut self, up: bool) {
        if matches!(self.screen, Screen::Chat) {
            let step = crate::screens::ChatState::SCROLL_STEP as isize;
            self.chat.scroll_by(if up { step } else { -step });
        }
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
                if settings.dialog.is_some() || settings.add_profile.is_some() {
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
            Routed::Resume(crate::screens::ResumeAction::Pick(path)) => {
                self.resume_session(&path);
                Vec::new()
            }
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
    fn perform_settings(&mut self, action: SettingsAction) -> Vec<Effect> {
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
            SettingsAction::EditSubagent => self.edit_subagent(),
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
    fn perform_onboarding(&mut self, action: OnboardingAction) -> Vec<Effect> {
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
                        wizard.go(crate::onboarding::OnboardingStep::Profile);
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
            OnboardingAction::RenameProfile(name) => {
                self.rename_session_profile(&name);
                if let Screen::Onboarding(wizard) = &mut self.screen {
                    wizard.profile_start = name.clone();
                    wizard.profile_name = name;
                    wizard.go(crate::onboarding::OnboardingStep::Appearance);
                }
                Vec::new()
            }
            OnboardingAction::Complete => {
                self.config.onboarded = Some(true);
                if let Err(err) = self.store.save(&self.config) {
                    self.error = Some(format!("cannot save config: {err}"));
                }
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
    fn on_chat_key(&mut self, key: KeyEvent, action: ChatAction) -> Vec<Effect> {
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
    fn steer_turn(&mut self, text: String) -> Vec<Effect> {
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

    /// The add-instance dialog validated a new instance's input: fetch
    /// the models; the fetch is the validation (a wrong key or a dead
    /// server both surface as the fetch's error). The dialog lives on
    /// the settings screen or the onboarding wizard — both run the same
    /// login flow.
    fn validate_login(&mut self) -> Vec<Effect> {
        let pending_input = match &self.screen {
            Screen::Settings(settings) => settings
                .dialog
                .as_ref()
                .map(|dialog| (dialog.pending.clone(), dialog.input.trim().to_owned())),
            Screen::Onboarding(wizard) => wizard
                .dialog
                .as_ref()
                .map(|dialog| (dialog.pending.clone(), dialog.input.trim().to_owned())),
            _ => None,
        };
        let Some((Some(pending), input)) = pending_input else {
            return Vec::new();
        };
        let base_url = pending
            .base_url
            .clone()
            .map(|url| url.trim().to_owned())
            .filter(|url| !url.is_empty());
        let backend = match pending.provider {
            crate::screens::LoginProvider::LlamaCpp => Backend::LlamaCpp {
                // The URL prompt was captured earlier; the current line
                // is the bearer key, optional on an open server.
                url: base_url.unwrap_or_default(),
                api_key: (!input.is_empty()).then_some(input),
            },
            crate::screens::LoginProvider::OpenAi => Backend::OpenAi {
                api_key: input,
                base_url,
            },
            crate::screens::LoginProvider::OpenRouter => Backend::OpenRouter { api_key: input },
            crate::screens::LoginProvider::Anthropic => Backend::Anthropic { api_key: input },
            crate::screens::LoginProvider::DeepSeek => Backend::DeepSeek { api_key: input },
            crate::screens::LoginProvider::Zai => Backend::Zai {
                api_key: input,
                base_url,
            },
            // The flow path never validates a typed key: its credential
            // arrives from the flow task, already wrapped in the backend.
            crate::screens::LoginProvider::Copilot => return Vec::new(),
            crate::screens::LoginProvider::Google => Backend::Google { api_key: input },
        };
        vec![Effect::FetchModels(FetchWhy::Login {
            backend,
            instance: Some(pending.name),
        })]
    }

    /// Enter on a logged-in instance: it becomes the profile's active
    /// one, and the agent rebuilds carrying the conversation over — a
    /// picker when no model is stored yet, the chat otherwise.
    fn activate_instance(&mut self) -> Vec<Effect> {
        if self.turn.running {
            self.transcript
                .push_notice("a turn is running — the switch waits for it to finish");
            return Vec::new();
        }
        let tab = match &self.screen {
            Screen::Settings(settings) => settings.profile.clone(),
            _ => return Vec::new(),
        };
        let Some(name) = (match &self.screen {
            Screen::Settings(settings) => settings.selected_instance().map(str::to_owned),
            _ => None,
        }) else {
            return Vec::new();
        };
        let Some(instance) = self
            .config
            .profiles
            .get(&tab)
            .and_then(|profile| profile.instances.get(&name).cloned())
        else {
            return Vec::new();
        };
        // A stored model id only carries over when the provider stayed
        // the same: gpt-x means nothing to a llama.cpp server.
        let previous_provider = self.config.effective(Some(&tab)).provider;
        let keep_model = previous_provider.as_deref() == Some(instance.provider.as_str());
        self.config.set_for_profile(Some(&tab), |settings| {
            settings.provider = Some(instance.provider.clone());
            settings.instance = Some(name.clone());
            if !keep_model {
                settings.model = None;
            }
        });
        if let Err(err) = self.store.save(&self.config) {
            self.error = Some(format!("cannot save config: {err}"));
            return Vec::new();
        }
        let effective = self.config.effective(Some(&tab));
        self.options.thinking = effective.thinking_effort();
        let Some(backend) = Backend::from_profile(&effective, self.secret_store()) else {
            self.error = Some(format!(
                "no usable credentials for {} — /login first",
                instance.provider
            ));
            return Vec::new();
        };
        let session = match &self.agent {
            Some(agent) => agent.session().clone(),
            None => self.session.clone(),
        };
        let label = format!("{} — {name}", backend.config_name());
        // The agent builds on the new provider, so it installs first.
        self.provider = Some(backend.provider());
        self.backend = Some(backend);
        let (saved, sections) = self.outgoing_transcript();
        let saved = saved.or(self.transcript_stats);
        // The model may have changed with the profile, and the cache is
        // per-model regardless: the report re-states its facts.
        let mut session = session;
        rhema_coding_agent::clear_environment_stamp(&mut session);
        if let Err(err) = self.rebuild_agent(session, saved, sections) {
            self.error = Some(err);
            return Vec::new();
        }
        self.session_profile = Some(tab);
        if let Some(model) = effective.model {
            self.screen = Screen::Chat;
            self.transcript
                .push_notice(&format!("switched to {label} · {model}"));
            // The session now rides the instance it activated:
            // that plan's numbers are the badge's to fetch.
            vec![Effect::FetchUsage]
        } else {
            self.screen = Screen::Picker(PickerScreen {
                status: PickerStatus::Loading,
                ..PickerScreen::new(self.models.clone())
            });
            vec![Effect::FetchModels(FetchWhy::Bootstrap), Effect::FetchUsage]
        }
    }

    /// `d` on a logged-in instance: forget it. When it was the active
    /// one, the backend re-resolves from what remains — another instance
    /// of the provider, or none.
    fn remove_instance(&mut self) -> Vec<Effect> {
        let tab = match &self.screen {
            Screen::Settings(settings) => settings.profile.clone(),
            _ => return Vec::new(),
        };
        let Some(name) = (match &self.screen {
            Screen::Settings(settings) => settings.selected_instance().map(str::to_owned),
            _ => None,
        }) else {
            return Vec::new();
        };
        // The keychain entry goes with the instance: a removed login
        // must not leave its key in the chain. Best effort — a store
        // that refuses leaves the entry; it holds no live reference.
        let mut removed = None;
        self.config.set_for_profile(Some(&tab), |settings| {
            removed = settings.remove_instance(&name);
        });
        // The catalog drops the instance's slice with it: nothing
        // re-fetches a removed login, and the next fetch's merge keeps
        // unqueried slices, so a stale copy would live forever.
        if let Some(old) = &removed {
            self.models.retain(|entry| {
                entry.provider != old.provider || entry.instance.as_deref() != Some(name.as_str())
            });
        }
        if let Some(id) = removed.and_then(|old| old.secret_ref)
            && let Some(store) = self.secret_store()
        {
            // The login leaves the config either way; a keychain
            // that refuses the delete leaves the secret behind, and
            // that must not pass silently.
            if let Err(err) = store.delete(&id) {
                self.error = Some(format!(
                    "the login is removed, but its keychain entry could not be deleted: {err}"
                ));
            }
        }
        if let Err(err) = self.store.save(&self.config) {
            self.error = Some(format!("cannot save config: {err}"));
            return Vec::new();
        }
        let mut effects = Vec::new();
        if self.session_profile.as_deref() == Some(tab.as_str()) {
            let effective = self.config.effective(Some(&tab));
            match Backend::from_profile(&effective, self.secret_store()) {
                Some(backend) => {
                    self.backend = Some(backend.clone());
                    self.provider = Some(backend.provider());
                    // What remains is a different plan: refresh the
                    // badge against it.
                    effects.push(Effect::FetchUsage);
                }
                None => {
                    // The agent drops with the provider, and its
                    // session comes home before the drop: the live
                    // history lands in `self.session`, so the exit
                    // save keeps this checkpoint instead of clobbering
                    // it with the stale copy held before the agent
                    // existed.
                    if let Some(agent) = self.agent.take() {
                        self.session = agent.session().clone();
                    }
                    self.backend = None;
                    self.provider = None;
                }
            }
        }
        // Sync the screen's copy of the profile and keep the selection
        // inside the shrunken list.
        if let Screen::Settings(settings) = &mut self.screen {
            settings.stored = self.config.effective(Some(&tab));
            let rows = settings.instance_rows().len();
            if settings.row >= rows {
                settings.row = rows.saturating_sub(1);
            }
        }
        self.transcript.push_notice(&format!("forgot {name}"));
        effects
    }

    /// A step of the running login flow: render it on the add-instance
    /// dialog — the settings' modal or the wizard's inline copy — and
    /// on completion validate the credential it produced through the
    /// same fetch path a typed key takes.
    fn on_login_update(&mut self, update: rhema_provider::LoginUpdate) -> Vec<Effect> {
        let dialog = match &mut self.screen {
            Screen::Settings(settings) => settings.dialog.as_mut(),
            Screen::Onboarding(wizard) => wizard.dialog.as_mut(),
            // The flow was left behind (Esc raced a completed update):
            // drop the credential with the screen it belonged to.
            _ => {
                self.login_cancel = None;
                return Vec::new();
            }
        };
        let Some(dialog) = dialog else {
            self.login_cancel = None;
            return Vec::new();
        };
        match update {
            rhema_provider::LoginUpdate::Instruct(instruction) => {
                dialog.flow_instruction = Some(instruction);
                dialog.flow_waiting = None;
                Vec::new()
            }
            rhema_provider::LoginUpdate::Waiting(text) => {
                dialog.flow_waiting = Some(text);
                Vec::new()
            }
            rhema_provider::LoginUpdate::Failed(message) => {
                dialog.status = crate::screens::DialogStatus::Error(message);
                // The flow task has ended; the handle can go.
                self.login_cancel = None;
                Vec::new()
            }
            rhema_provider::LoginUpdate::Completed(token) => {
                dialog.flow_waiting = Some("fetching the model list".to_owned());
                self.login_cancel = None;
                // The flow's instance was named before the flow started.
                let instance = dialog.pending.as_ref().map(|p| p.name.clone());
                vec![Effect::FetchModels(FetchWhy::Login {
                    backend: Backend::Copilot { api_key: token },
                    instance,
                })]
            }
        }
    }

    /// Retry a failed model fetch.
    fn retry_fetch(&mut self) -> Vec<Effect> {
        match &self.screen {
            Screen::Picker(_) => vec![Effect::FetchModels(FetchWhy::Refresh)],
            _ => Vec::new(),
        }
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

    /// Handles one event from the turn/fetch tasks.
    pub fn on_event(&mut self, event: UiEvent) -> Vec<Effect> {
        match event {
            UiEvent::Engine(owned) => {
                self.track_phase(&owned);
                self.transcript.apply(&owned);
                Vec::new()
            }
            UiEvent::ModelsLoaded { why, result } => self.on_models_loaded(why, result),
            UiEvent::LoginUpdate(update) => self.on_login_update(update),
            UiEvent::ApprovalRequested { call } => {
                if self.turn.decline_all {
                    // Esc declined the rest of the turn: answer without a
                    // modal so the blocked turn task keeps going.
                    self.turn.answer(false);
                } else {
                    self.turn.pending_approval = Some(call);
                }
                Vec::new()
            }
            UiEvent::UsageLoaded { provider, result } => {
                // Err or a switched-away backend both degrade to a
                // smaller badge, never to a stale figure.
                self.subscription = result.ok().map(|allowance| (provider, allowance));
                Vec::new()
            }
            UiEvent::SandboxAssembled { choice, result } => {
                self.assembling = None;
                match result {
                    Ok(assembled) => {
                        self.replace_sandboxed(Some(assembled));
                        self.rebuild_for_sandbox(&format!(
                            "bash runs on {} now",
                            choice.stored_name()
                        ));
                    }
                    Err(err) => self.transcript.push_notice(&format!(
                        "the {} sandbox failed: {err}; bash stays on {}",
                        choice.stored_name(),
                        self.active_sandbox().stored_name()
                    )),
                }
                Vec::new()
            }
            UiEvent::TurnFinished {
                agent,
                result,
                steer,
            } => {
                // The turn just moved the account's counters; the badge
                // is only as good as its last fetch. A steered message
                // that raced the finish rides along as the next turn.
                let mut effects = self.end_turn(agent, result, steer);
                effects.push(Effect::FetchUsage);
                effects
            }
            UiEvent::TurnCancelled { agent, steer } => {
                self.cancel_finished(agent, steer);
                vec![Effect::FetchUsage]
            }
        }
    }

    /// The redraw loop's per-tick work: subagent finishes reach the
    /// transcript, and a finished job's notification reaches the model.
    /// Both deliveries split on the turn state — the steering path and
    /// the regular-prompt path:
    ///
    /// - a finish lands as a transcript notice right away, whatever
    ///   the turn state — the job's pinned line left the strip at the
    ///   same moment, so the notice is the record;
    /// - while a turn runs, the driver steers: it appends the pending
    ///   notification lines at the next step boundary, and the tick
    ///   touches nothing but the notice;
    /// - with no turn running, the tick takes the pending lines and
    ///   runs them as the next turn's prompt — the same delivery a
    ///   user submit would give them. The take retires the lines, so
    ///   the driver's turn-start drain does not repeat what the prompt
    ///   already said, and the prompt's user line is the transcript's
    ///   record (no separate notice for the same text).
    pub fn on_tick(&mut self) -> Vec<Effect> {
        let Some(handle) = self.subagents.as_ref() else {
            return Vec::new();
        };
        let finishes = handle.take_finished();
        let promptable = !self.turn.running
            && !self.confirm_quit
            && self.agent.is_some()
            && self.assembling.is_none();
        if !promptable {
            for job in finishes {
                self.transcript.push_notice(&job.line);
            }
            return Vec::new();
        }
        let lines = handle.take_notifications();
        if lines.is_empty() {
            return Vec::new();
        }
        let prompt = lines.join("\n");
        self.transcript.push_user(&prompt, Vec::new());
        vec![Effect::StartTurn {
            text: prompt,
            images: Vec::new(),
        }]
    }

    /// Persists a validated login: the instance lands in `tab`'s profile
    /// (its key in the keychain when the store takes it, the file when
    /// it does not; a re-login's old keychain entry goes first), the
    /// session rides the validated backend, and the catalog's slice for
    /// the instance replaces itself. The error, if any, is for the
    /// dialog to render inline.
    fn install_login(
        &mut self,
        tab: &str,
        instance: &str,
        backend: &Backend,
        models: &[ModelCard],
    ) -> Result<(), String> {
        let provider = backend.config_name();
        let (api_key, base_url) = match backend {
            Backend::LlamaCpp { url, api_key } => (api_key.clone(), Some(url.clone())),
            Backend::OpenAi { api_key, base_url } | Backend::Zai { api_key, base_url } => {
                (Some(api_key.clone()), base_url.clone())
            }
            Backend::Google { api_key }
            | Backend::OpenRouter { api_key }
            | Backend::Anthropic { api_key }
            | Backend::DeepSeek { api_key }
            | Backend::Copilot { api_key } => (Some(api_key.clone()), None),
        };
        let secrets = self.secrets.as_deref();
        let mut keychain_delete_failure: Option<String> = None;
        self.config.set_for_profile(Some(tab), |settings| {
            // A re-login replaces the instance wholesale; its old
            // keychain entry must not linger — but a store that refuses
            // the delete must not pass silently either, or the secret
            // outlives the login that named it.
            if let Some(old) = settings.instances.get(instance)
                && let Some(id) = old.secret_ref.as_deref()
                && let Some(store) = secrets
                && let Err(err) = store.delete(id)
            {
                keychain_delete_failure = Some(err.to_string());
            }
            settings.provider = Some(provider.to_owned());
            settings.instance = Some(instance.to_owned());
            settings.upsert_instance(
                instance,
                rhema_config::ProviderInstance {
                    provider: provider.to_owned(),
                    api_key: None,
                    secret_ref: None,
                    base_url,
                },
            );
            if let Some(key) = api_key.as_deref() {
                settings.place_key(instance, key, secrets);
            }
        });
        if let Some(err) = keychain_delete_failure {
            self.error = Some(format!(
                "re-logged in, but the previous keychain entry could not be deleted: {err}"
            ));
        }
        self.store
            .save(&self.config)
            .map_err(|err| format!("cannot save config: {err}"))?;
        self.session_profile = Some(tab.to_owned());
        self.backend = Some(backend.clone());
        self.provider = Some(backend.provider());
        // The other logins' catalogs stay: a fresh login must not blank
        // the picker down to one provider. The instance's own slice
        // replaces itself, so a stale copy cannot linger — a provider
        // may hold several.
        self.models.retain(|entry| {
            entry.provider != provider || entry.instance.as_deref() != Some(instance)
        });
        self.models
            .extend(models.iter().map(|model| crate::screens::CatalogModel {
                provider,
                instance: Some(instance.to_owned()),
                model: model.clone(),
            }));
        self.models.sort_by(|a, b| {
            a.provider
                .cmp(b.provider)
                .then(a.instance.cmp(&b.instance))
                .then(a.model.id.cmp(&b.model.id))
        });
        Ok(())
    }

    /// Routes a model-list result to the login screen, the open picker,
    /// or the cache.
    fn on_models_loaded(
        &mut self,
        why: FetchWhy,
        result: Vec<crate::events::CatalogFetch>,
    ) -> Vec<Effect> {
        match why {
            FetchWhy::Login { backend, instance } => {
                // Every login writes a named instance; a lost name (a
                // flow that outlived its screen) falls back to the
                // provider's own.
                let instance = instance.unwrap_or_else(|| backend.config_name().to_owned());
                // The login fetch validates one provider: its result is
                // the catalog's only entry.
                match result.into_iter().next().map(|entry| entry.result) {
                    Some(Ok(models)) => {
                        let provider = backend.config_name();
                        // Both login hosts edit the session's profile:
                        // the settings' Providers page and the wizard's
                        // step are only different faces of it.
                        let tab = match &self.screen {
                            Screen::Settings(settings) => settings.profile.clone(),
                            Screen::Onboarding(_) => self
                                .session_profile
                                .clone()
                                .unwrap_or_else(|| rhema_config::DEFAULT_PROFILE.to_owned()),
                            // The dialog was left behind (Esc raced a
                            // completed fetch): the login has nowhere
                            // to land.
                            _ => return Vec::new(),
                        };
                        if let Err(err) = self.install_login(&tab, &instance, &backend, &models) {
                            match &mut self.screen {
                                Screen::Settings(settings) => {
                                    if let Some(dialog) = &mut settings.dialog {
                                        dialog.status =
                                            crate::screens::DialogStatus::Error(err.clone());
                                    }
                                }
                                Screen::Onboarding(wizard) => {
                                    if let Some(dialog) = &mut wizard.dialog {
                                        dialog.status =
                                            crate::screens::DialogStatus::Error(err.clone());
                                    }
                                }
                                _ => {}
                            }
                            return Vec::new();
                        }
                        // The dialog served its purpose; the wizard's
                        // copy goes too (the model step renders from its
                        // own rows).
                        if let Screen::Settings(settings) = &mut self.screen {
                            settings.dialog = None;
                        }
                        // Where the login lands depends on the screen it
                        // came from: the settings flow opens the picker
                        // (skipping straight to the chat on a
                        // single-model server), the wizard advances to
                        // its own model step (or takes the single model
                        // straight to the profile step).
                        if let Screen::Onboarding(wizard) = &mut self.screen {
                            wizard.backend_ready = true;
                            wizard.stored = self.config.effective(Some(&tab));
                            // The dialog served its purpose; its keys
                            // must not eat the model step's.
                            wizard.dialog = None;
                        }
                        if matches!(self.screen, Screen::Onboarding(_)) {
                            if models.len() == 1 {
                                let id = models[0].id.clone();
                                return match self.apply_model_choice(provider, Some(&instance), &id)
                                {
                                    Ok(effects) => {
                                        if let Screen::Onboarding(wizard) = &mut self.screen {
                                            wizard.chosen_model = Some(id);
                                            wizard.chosen_provider = Some(provider.to_owned());
                                            wizard.go(crate::onboarding::OnboardingStep::Profile);
                                        }
                                        effects
                                    }
                                    Err(err) => {
                                        if let Screen::Onboarding(wizard) = &mut self.screen {
                                            wizard.error = Some(err);
                                        }
                                        Vec::new()
                                    }
                                };
                            }
                            let rows: Vec<crate::screens::CatalogModel> = self
                                .models
                                .iter()
                                .filter(|entry| {
                                    entry.provider == provider
                                        && entry.instance.as_deref() == Some(instance.as_str())
                                })
                                .cloned()
                                .collect();
                            if let Screen::Onboarding(wizard) = &mut self.screen {
                                wizard.models = rows;
                                wizard.go(crate::onboarding::OnboardingStep::Model);
                            }
                            return vec![Effect::FetchUsage];
                        }
                        // A single-model server (plain llama-server rather
                        // than a router) has nothing to pick: take it and
                        // go straight to chat.
                        if models.len() == 1 {
                            let id = models[0].id.clone();
                            return self.choose_model(provider, Some(&instance), &id);
                        }
                        self.screen = Screen::Picker(PickerScreen::new(self.models.clone()));
                        // A login installs the backend it validated: that
                        // plan is the session's numbers now.
                        vec![Effect::FetchUsage]
                    }
                    Some(Err(message)) => {
                        match &mut self.screen {
                            Screen::Settings(settings) => {
                                if let Some(dialog) = &mut settings.dialog {
                                    dialog.on_validated(&Err(message));
                                }
                            }
                            Screen::Onboarding(wizard) => {
                                if let Some(dialog) = &mut wizard.dialog {
                                    dialog.on_validated(&Err(message));
                                }
                            }
                            _ => {}
                        }
                        Vec::new()
                    }
                    // The dispatch always spawns one backend per login;
                    // an empty result cannot arrive.
                    None => {
                        let message = "no provider answered".to_owned();
                        match &mut self.screen {
                            Screen::Settings(settings) => {
                                if let Some(dialog) = &mut settings.dialog {
                                    dialog.on_validated(&Err(message));
                                }
                            }
                            Screen::Onboarding(wizard) => {
                                if let Some(dialog) = &mut wizard.dialog {
                                    dialog.on_validated(&Err(message));
                                }
                            }
                            _ => {}
                        }
                        Vec::new()
                    }
                }
            }
            FetchWhy::Bootstrap | FetchWhy::Refresh => {
                // One result per queried backend: each fetched slice
                // replaces itself in the catalog, and a slice no fetch
                // asked about — a login that raced this one — survives. A
                // total failure is the picker's error, and a partial one
                // is named in its footer — a missing provider must never
                // look like an empty one.
                let (catalogs, failed): (Vec<_>, Vec<_>) =
                    result.into_iter().partition(|entry| entry.result.is_ok());
                for entry in catalogs.iter().chain(failed.iter()) {
                    self.models.retain(|existing| {
                        existing.provider != entry.provider
                            || existing.instance.as_deref() != entry.instance.as_deref()
                    });
                }
                self.models.extend(catalogs.iter().flat_map(|entry| {
                    let models = entry.result.as_deref().unwrap_or_default();
                    models
                        .iter()
                        .map(move |model| crate::screens::CatalogModel {
                            provider: entry.provider,
                            instance: entry.instance.clone(),
                            model: model.clone(),
                        })
                }));
                self.models.sort_by(|a, b| {
                    a.provider
                        .cmp(b.provider)
                        .then(a.instance.cmp(&b.instance))
                        .then(a.model.id.cmp(&b.model.id))
                });
                if let Screen::Picker(picker) = &mut self.screen {
                    if catalogs.is_empty() {
                        let message = failed
                            .into_iter()
                            .next()
                            .and_then(|entry| entry.result.err())
                            .unwrap_or_else(|| "no provider answered".to_owned());
                        picker.status = PickerStatus::Error(message);
                    } else {
                        picker.set_models(self.models.clone());
                        picker.failed = failed.into_iter().map(|entry| entry.provider).collect();
                        picker.status = PickerStatus::Ready;
                    }
                }
                Vec::new()
            }
        }
    }

    /// A model was named for `/model <needle>`: resolve the needle against
    /// the cached catalog — exact id first, then a unique prefix — and
    /// switch, or say what went missing. `/model` with no needle opens
    /// the full picker instead.
    pub fn choose_model_by_id(&mut self, needle: &str) -> Vec<Effect> {
        // The active instance wins ties: the same model id may be served
        // by several instances of one provider.
        let active = self.effective().instance;
        let mut tag: Vec<(&'static str, Option<String>, String)> = self
            .models
            .iter()
            .filter(|entry| entry.model.id == needle)
            .map(|entry| {
                (
                    entry.provider,
                    entry.instance.clone(),
                    entry.model.id.clone(),
                )
            })
            .collect();
        let prefix: Vec<(&'static str, Option<String>, String)> = self
            .models
            .iter()
            .filter(|entry| entry.model.id.starts_with(needle))
            .map(|entry| {
                (
                    entry.provider,
                    entry.instance.clone(),
                    entry.model.id.clone(),
                )
            })
            .collect();
        let (exact_count, prefix_count) = (tag.len(), prefix.len());
        let resolved = if exact_count == 1 {
            tag.pop()
        } else if exact_count > 1 {
            tag.into_iter().find(|(_, instance, _)| *instance == active)
        } else if prefix_count == 1 {
            prefix.into_iter().next()
        } else if prefix_count > 1 {
            prefix
                .into_iter()
                .find(|(_, instance, _)| *instance == active)
        } else {
            None
        };
        match resolved {
            Some((provider, instance, id)) => self.choose_model(provider, instance.as_deref(), &id),
            None if exact_count > 1 || prefix_count > 1 => {
                self.push_notice(&format!(
                    "{} models match {needle:?}; name one fully or use /model",
                    exact_count.max(prefix_count)
                ));
                Vec::new()
            }
            None => {
                self.push_notice(&format!(
                    "no model matches {needle:?} in the cached catalog; /model lists them"
                ));
                Vec::new()
            }
        }
    }

    /// A model was chosen in the picker: persist it and (re)build the
    /// agent, carrying the current session over, and land in the chat.
    /// The old agent is only dropped once the new one built, so a
    /// rebuild failure cannot strand the chat.
    fn choose_model(
        &mut self,
        provider: &'static str,
        instance: Option<&str>,
        id: &str,
    ) -> Vec<Effect> {
        let from_chat =
            matches!(&self.screen, Screen::Picker(_)) || matches!(&self.screen, Screen::Chat);
        match self.apply_model_choice(provider, instance, id) {
            Ok(effects) => {
                // An interactive switch — the picker opened from the
                // chat, or a `/model <needle>` typed there — says where
                // it landed; the login auto-selects stay quiet.
                if from_chat {
                    self.transcript
                        .push_notice(&format!("model switched to {provider}/{id}"));
                }
                self.screen = Screen::Chat;
                effects
            }
            Err(err) => {
                self.error = Some(err);
                Vec::new()
            }
        }
    }

    /// The model choice without the screen move: the pick persists into
    /// the session's profile, the backend re-resolves, and the agent
    /// rebuilds carrying the conversation over. The chat's picker path
    /// and the onboarding wizard's step share it; each lands where it
    /// needs to afterwards.
    fn apply_model_choice(
        &mut self,
        provider: &str,
        instance: Option<&str>,
        id: &str,
    ) -> Result<Vec<Effect>, String> {
        let session = match &self.agent {
            Some(agent) => agent.session().clone(),
            None => self.session.clone(),
        };
        // The write follows the session's profile, not the startup flag:
        // after a mid-session /profile the pick must land where effective()
        // reads it back from — profiles never inherit each other's model.
        // The provider — and the instance it came from — moves with the
        // model: a pick from another login's catalog switches the backend
        // this session rides on.
        let profile = self.session_profile.as_deref();
        self.config.set_for_profile(profile, |settings| {
            settings.provider = Some(provider.to_owned());
            settings.instance = instance.map(str::to_owned);
            settings.model = Some(id.to_owned());
        });
        if let Err(err) = self.store.save(&self.config) {
            return Err(format!("cannot save config: {err}"));
        }
        let backend = match Backend::from_profile(&self.effective(), self.secret_store()) {
            Some(backend) => backend,
            None => {
                return Err(format!(
                    "no usable credentials for {provider}; /login first"
                ));
            }
        };
        self.backend = Some(backend.clone());
        self.provider = Some(backend.provider());
        let (saved, sections) = self.outgoing_transcript();
        let saved = saved.or(self.transcript_stats);
        // The model may have changed, and the cache is per-model
        // regardless: the report re-states its facts.
        let mut session = session;
        rhema_coding_agent::clear_environment_stamp(&mut session);
        self.rebuild_agent(session, saved, sections)?;
        // A new backend counts a new plan: the allowance badge fetches
        // now, not at the first turn's end.
        Ok(vec![Effect::FetchUsage])
    }

    /// The outgoing agent's counters and sections, read before a
    /// rebuild: the pair [`Self::rebuild_agent`] carries onto the new
    /// agent, so the settings pages keep reading as one run.
    fn outgoing_transcript(
        &self,
    ) -> (
        Option<rhema_transcript::TranscriptStats>,
        Vec<rhema_transcript::Section>,
    ) {
        (
            self.agent.as_ref().and_then(CodingAgent::transcript_stats),
            self.agent
                .as_ref()
                .map(CodingAgent::transcript_sections)
                .unwrap_or_default(),
        )
    }

    /// Swaps the session's agent for one built over `session` under
    /// the current profile and flags — the rebuild idiom. The
    /// transcript counters and sections given here ride along: same
    /// conversation, new executor. `Err` carries the build failure's
    /// text for the caller to notice with.
    fn rebuild_agent(
        &mut self,
        session: Session,
        stats: Option<rhema_transcript::TranscriptStats>,
        sections: Vec<rhema_transcript::Section>,
    ) -> Result<(), String> {
        let (mut agent, subagents) = self.build_agent(session).map_err(|err| err.to_string())?;
        if let Some(stats) = &stats {
            agent.carry_transcript_stats(stats);
        }
        agent.adopt_transcript_sections(sections);
        self.agent = Some(agent);
        self.subagents = Some(subagents);
        Ok(())
    }

    /// Rebuilds the live agent in place: take it, carry its
    /// transcript, build over its session, reinstall. `stamp` clears
    /// the session's environment stamp first — for callers whose
    /// change alters what the machine reports. No agent, nothing to
    /// rebuild.
    fn rebuild_live_agent(&mut self, stamp: bool) -> Result<(), String> {
        let Some(agent) = self.agent.take() else {
            return Ok(());
        };
        let stats = agent.transcript_stats();
        let sections = agent.transcript_sections();
        let mut session = agent.session().clone();
        if stamp {
            rhema_coding_agent::clear_environment_stamp(&mut session);
        }
        self.rebuild_agent(session, stats, sections)
    }

    /// Builds an agent over `session` with the current model and flags.
    /// The wire's registry handle rides beside the agent: a rebuilt
    /// agent carries a fresh registry, and the UI must read that one.
    fn build_agent(
        &self,
        session: Session,
    ) -> Result<(CodingAgent, rhema_subagents::SubagentHandle), CodingAgentError> {
        let provider = self
            .provider
            .clone()
            .ok_or(CodingAgentError::Agent(AgentError::MissingProvider))?;
        let (agent_config, subagents) = session_agent_config(
            &self.options,
            self.sandboxed.as_ref(),
            self.effective().model.clone(),
            provider.clone(),
            &self.config.skills_disabled.clone(),
        );
        Ok((
            CodingAgent::from_session(provider, agent_config, session)?,
            subagents,
        ))
    }

    /// The runner accepted a turn: a fresh lifecycle state with the
    /// cancel flag and the approval answer channel wired, starting in
    /// the thinking phase.
    pub fn begin_turn(
        &mut self,
        cancel: tokio::sync::watch::Sender<bool>,
        steer: tokio::sync::mpsc::Sender<String>,
        answers: Option<ApprovalAnswerer>,
    ) {
        self.turn = TurnState::running(cancel, steer, answers);
        self.remember_profile();
    }

    /// Records the session's profile as the last one this workspace
    /// chose or ran with — an explicit `/profile` switch or a turn's
    /// start — so the next launch here opens on it. Written only on
    /// change; a failure surfaces in the status strip like every other
    /// config write — neither the switch nor the turn is blocked by it.
    fn remember_profile(&mut self) {
        let Some(profile) = self.session_profile.clone() else {
            return;
        };
        let workspace = self.options.workspace.clone();
        if self.config.last_profile_in(&workspace).as_deref() == Some(profile.as_str()) {
            return;
        }
        self.config.note_profile_in(&workspace, &profile);
        if let Err(err) = self.store.save(&self.config) {
            self.error = Some(format!("cannot save config: {err}"));
        }
    }

    /// Tracks which phase a running turn is in, from the engine's own
    /// events: streaming is thinking, a tool call is processing. A result
    /// leaves the phase alone — the next call or step moves it, so the
    /// label never flickers between back-to-back commands. Reasoning
    /// deltas fill the preview buffer; real output or a tool call clears
    /// it — the thinking block vanishes once the answer starts. A
    /// running command's lines fill the tool preview; its result or the
    /// next call clears that. The provider's running usage tally lands
    /// in `usage` as it arrives: its prompt count already covers the
    /// whole conversation, so the footer's gauge moves with the stream
    /// instead of waiting for `StepEnd`.
    fn track_phase(&mut self, owned: &OwnedEvent) {
        // Any event ends a retry wait: the next attempt is speaking.
        // The announcement itself is what sets one.
        if !matches!(owned, OwnedEvent::RetryWait { .. }) {
            self.turn.retry_wait = None;
        }
        match owned {
            OwnedEvent::StepStart { .. } | OwnedEvent::TextDelta(_) => {
                self.turn.processing = false;
                self.turn.thinking.clear();
            }
            OwnedEvent::ThinkingDelta(delta) => self.turn.push_thinking(delta),
            // A tool call means a command runs. Its arguments often never
            // stream as deltas, so the label stays phase-only; the last
            // command's preview goes with it.
            OwnedEvent::ToolCallStart { .. } => {
                self.turn.processing = true;
                self.turn.thinking.clear();
                self.turn.clear_tool_output();
            }
            OwnedEvent::ToolOutputDelta { chunk, .. } => self.turn.push_tool_output(chunk),
            // A background subagent job reports through the registry
            // (the wire's handle), not through the call's own status
            // stream: the start call answers with its job id at once.
            OwnedEvent::ToolStatus { .. } => {}
            OwnedEvent::ToolResult { .. } => self.turn.clear_tool_output(),
            // A zero tally is a placeholder frame, not a measurement:
            // keeping the stale figure beats dropping the gauge to 0.
            OwnedEvent::Usage(usage) if context_tokens(usage) > 0 => self.usage = Some(*usage),
            OwnedEvent::RetryWait {
                error,
                delay,
                attempt,
                total,
            } => {
                self.turn.retry_wait = Some(TurnRetryWait {
                    error: error.clone(),
                    deadline: std::time::Instant::now() + *delay,
                    attempt: *attempt,
                    total: *total,
                });
            }
            _ => {}
        }
    }

    /// Cancels the running turn from the keyboard. The approval bridge
    /// blocks its thread on the answer, so a pending modal is released
    /// first; upcoming approvals are declined; then the flag flips and
    /// the turn task drops the future at its current await. The agent
    /// comes back in a `TurnCancelled`.
    fn cancel_turn(&mut self) {
        self.turn.answer(false);
        self.turn.decline_all = true;
        if let Some(cancel) = &self.turn.cancel {
            let _ = cancel.send(true);
        }
    }

    /// The turn task was cancelled from the keyboard: take the agent
    /// back, drop the turn's state, and mark the cutoff in the transcript.
    fn cancel_finished(
        &mut self,
        agent: Box<CodingAgent>,
        mut steer: tokio::sync::mpsc::Receiver<String>,
    ) {
        self.transcript_stats = agent.transcript_stats();
        self.agent = Some(*agent);
        self.turn.finished();
        self.retryable = false;
        self.transcript.push_notice("turn cancelled");
        // A steered message that raced the cancel goes back to the input:
        // the user stopped the turn, so the words are not spent.
        let mut restore = String::new();
        while let Ok(text) = steer.try_recv() {
            if !restore.is_empty() {
                restore.push('\n');
            }
            restore.push_str(&text);
        }
        // The queued chain dies with the turn the same way: unspent
        // words go back to the input, in order.
        while let Some(text) = self.pending_prompts.pop_front() {
            if !restore.is_empty() {
                restore.push('\n');
            }
            restore.push_str(&text);
        }
        if !restore.is_empty() {
            self.chat.insert_str(&restore);
        }
    }

    /// The turn task finished: take the agent back, record the usage or
    /// the error, and drop any stale modal (the turn cannot finish while
    /// an approval is unanswered, so a leftover modal means the answer
    /// channel is gone).
    fn end_turn(
        &mut self,
        agent: Box<CodingAgent>,
        result: Result<ModelOutput, String>,
        mut steer: tokio::sync::mpsc::Receiver<String>,
    ) -> Vec<Effect> {
        self.transcript_stats = agent.transcript_stats();
        self.agent = Some(*agent);
        self.turn.finished();
        match result {
            Ok(output) => {
                self.retryable = false;
                if let Some(pricing) = self.current_model_pricing() {
                    let usage = output.usage;
                    self.session_cost += pricing.cost(
                        u64::from(usage.input_tokens),
                        u64::from(usage.output_tokens),
                        u64::from(usage.cached_input_tokens),
                    );
                }
                // A final tally of zeros is a proxy that never wired
                // usage through, not a free turn: keep the previous
                // gauge instead of pricing the turn at nothing.
                if output.usage.input_tokens > 0 || output.usage.output_tokens > 0 {
                    self.usage = Some(output.usage);
                }
                // Silence is a symptom, not an answer: the usual cause is
                // a reasoning-only response — a thinking model spending
                // its whole output budget before any text started — and
                // the turn must not read as completed when nothing was
                // said.
                let said_something = output.message.content.iter().any(|content| {
                    matches!(content, Content::Text(text) if !text.is_empty())
                        || matches!(content, Content::ToolCall(_))
                });
                if !said_something {
                    self.push_notice(&format!(
                        "the model returned no content (stopped: {})",
                        Self::stop_words(&output.stop_reason)
                    ));
                }
            }
            Err(message) => {
                self.error = Some(message);
                self.retryable = true;
            }
        }
        // Messages sent in the finishing race sit in the returned
        // receiver: they run as the next turns, in order. The transcript
        // already carries their user lines from submit time. Only one
        // turn leaves per finish — the dispatch takes the agent once,
        // and a second StartTurn would find it gone — so the rest queue
        // on the app and each finish chains the next one out.
        while let Ok(text) = steer.try_recv() {
            self.pending_prompts.push_back(text);
        }
        let mut effects = Vec::new();
        if let Some(text) = self.pending_prompts.pop_front() {
            self.retryable = false;
            effects.push(Effect::StartTurn {
                text,
                images: Vec::new(),
            });
        }
        effects
    }

    /// The stop reason as the few words a notice names it by.
    fn stop_words(reason: &StopReason) -> String {
        match reason {
            StopReason::EndTurn => "end turn".to_owned(),
            StopReason::ToolUse => "tool use".to_owned(),
            StopReason::MaxTokens => "max tokens".to_owned(),
            StopReason::StopSequence => "stop sequence".to_owned(),
            StopReason::Other(other) => other.clone(),
        }
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
    fn current_model_pricing(&self) -> Option<rhema_provider::Pricing> {
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
    fn live_session(&self) -> &Session {
        match &self.agent {
            Some(agent) => agent.session(),
            None => &self.session,
        }
    }
}

/// The coding-agent config implied by the startup flags and the chosen
/// model. `disabled` carries the config's skill off-switch through to
/// the prompt's skills list.
#[cfg(test)]
mod tests;
