//! rhema-tui — the full-screen terminal UI for the rhema coding agent.
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
//! - [`events`] — the UI event protocol and owned event copies
//! - [`store`] — the config persistence seam
//! - [`approval`] — the synchronous approval bridge
//! - [`transcript`] — chat transcript lines from history and events
//! - [`screens`] — login, model picker, chat input: pure state
//! - [`app`] — the state machine routing keys and events
//! - [`turn`] — the tokio tasks (turn, model fetch)
//! - [`ui`] — ratatui rendering
//! - `run` — terminal setup/teardown and the event loop

#![forbid(unsafe_code)]
#![deny(missing_docs)]

pub mod app;
pub mod approval;
pub mod args;
pub mod events;
pub mod markdown;
pub mod palette;
pub mod run;
pub mod screens;
pub mod store;
pub mod transcript;
pub mod turn;
pub mod ui;

pub use app::{App, AppSetup, Effect, RunOptions, Screen};
pub use approval::{ApprovalAnswerer, TuiApproval};
pub use args::Args;
pub use events::{FetchWhy, OwnedEvent, UiEvent};
pub use palette::{DARK, LIGHT, Theme};
pub use run::run;
pub use screens::{ChatAction, ChatState, LoginAction, LoginScreen, PickerAction, PickerScreen};
pub use store::{ConfigStore, DiskStore, MemoryStore};
pub use transcript::{Line, Transcript};
