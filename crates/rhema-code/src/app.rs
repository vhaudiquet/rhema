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
use crate::events::{FetchWhy, OwnedEvent, ResumedLoad, UiEvent};
use crate::onboarding::{OnboardingAction, OnboardingScreen};
use crate::screens::{
    AddInstanceDialog, ChatAction, ChatState, PickerAction, PickerScreen, PickerStatus,
    ProfileAction, ProfileScreen, SettingsAction, SettingsScreen, SettingsSection, SkillRow,
    SkillSource, SubagentRow, SubagentSource,
};
use crate::store::ConfigStore;
use crate::transcript::Transcript;

mod input;
mod lifecycle;
mod model;
mod profiles;
mod session;
mod settings;
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
/// then. Between turns everything rests at its `TurnState::idle` default.
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
}

#[cfg(test)]
mod tests;
