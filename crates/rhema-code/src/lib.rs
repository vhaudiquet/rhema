//! rhema — the frontend for the rhema coding agent: a full-screen
//! terminal UI when attached to a terminal, a pipeable one-shot runner
//! when not.
//!
//! A leaf consumer: all logic lives in the libraries, this crate is screens
//! and event plumbing only. `main.rs` is binary wiring; everything else
//! lives here so it stays testable.
//!
//! Concurrency shape: the UI thread owns the terminal and runs a loop of
//! draw → drain-channel → poll-crossterm. A turn runs on a tokio task and
//! reports through an unbounded mpsc channel of [`events::UiEvent`]s;
//! tool-call approvals block the turn task on a std mpsc answer channel
//! that only the UI thread writes (see [`approval`] for the deadlock
//! argument).
//!
//! Layout:
//! - [`args`] — clap flags (no `--model`, no `--api-key`)
//! - [`clipboard`] — the system clipboard, for ctrl-v as a paste key
//! - [`events`] — the UI event protocol and owned event copies
//! - [`store`] — the config persistence seam
//! - [`approval`] — the synchronous approval bridge
//! - [`transcript`] — chat transcript lines from history and events
//! - [`screens`] — login, model picker, chat input: pure state
//! - [`onboarding`] — the first-run wizard's steps and keys
//! - [`bundled`] — the skills rhema ships with, seeded on start
//! - [`sessions`] — saved conversations for /resume, grouped by workspace
//! - [`status`] — program status over OSC 7501, for the terminal's tabs
//! - [`app`] — the state machine routing keys and events
//! - [`turn`] — the tokio tasks (turn, model fetch)
//! - [`ui`] — ratatui rendering
//! - [`chrome`] — the shared panel/marker vocabulary of the menus
//! - [`term_image`] — the onboarding logo through the kitty graphics
//!   protocol (kitty and Ghostty)
//! - [`palette`] — color palettes: named token sets in two appearances;
//!   [`hct`] and `m3` (private) carry the color math and the scheme
//!   behind M3 You
//! - [`sandbox`] — where `bash` runs: the sandbox choice's resolution
//!   and the assembly behind it
//! - `run` — terminal setup/teardown and the event loop

#![forbid(unsafe_code)]

/// The transcript banner's version label, baked by `build.rs` from the
/// tree the binary was built from: `v0.1.0` for a clean checkout at the
/// matching tag (or a build outside any git tree), `v0.1.0-edge.<sha>`
/// on the edge channel, `v0.1.0+<sha>` for a clean devel build,
/// `v0.1.0+<sha>.dirty` with local changes.
pub const VERSION_LABEL: &str = env!("RHEMA_VERSION_LABEL");

pub mod app;
pub mod approval;
pub mod args;
pub mod bundled;
pub mod chrome;
pub mod clipboard;
pub mod commands;
pub mod events;
mod hct;
pub mod icons;
mod m3;
pub mod markdown;
pub mod onboarding;
pub mod palette;
pub mod run;
pub mod sandbox;
pub mod screens;
pub mod sessions;
pub mod status;
pub mod store;
pub mod term_image;
pub mod transcript;
pub mod turn;
pub mod ui;

pub use app::{App, AppSetup, Effect, RunOptions, Screen};
pub use approval::ApprovalAnswerer;
pub use args::Args;
pub use events::{FetchWhy, OwnedEvent, UiEvent};
pub use onboarding::{OnboardingAction, OnboardingScreen, OnboardingStep};
pub use palette::{DARK, LIGHT, Palette, Theme, ThemeKind, parse_hex_color};
pub use run::{exit_code, run};
pub use screens::{
    AddInstanceDialog, ChatAction, ChatState, PickerAction, PickerScreen, ResumeAction,
    ResumeScreen, SettingsAction, SettingsScreen, SettingsSection,
};
pub use store::{ConfigStore, DiskStore, MemoryStore};
pub use transcript::{Line, Transcript};
