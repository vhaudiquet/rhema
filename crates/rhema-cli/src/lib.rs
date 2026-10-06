//! rhema-cli — the `rhema` binary's terminal UI.
//!
//! Argument wiring lives in `main.rs`; everything else lives here so it
//! stays testable: [`args::Args`] (clap), [`sink::TermSink`] (the streaming
//! terminal output), [`approval::ConsoleApproval`] (the interactive
//! approval hook), and `run` (session setup, one-shot mode, the REPL).
//!
//! All agent logic is in `rhema-coding-agent`; all model transport is in
//! `rhema-provider-google`. This crate is wiring and rendering only.

#![forbid(unsafe_code)]
#![deny(missing_docs)]

pub mod approval;
pub mod args;
pub mod run;
pub mod sink;

pub use args::Args;
pub use run::run;
