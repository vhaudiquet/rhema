//! The command-line surface, parsed with clap's derive API. Deliberately
//! narrower than the rhema CLI: the model is owned by the picker and the
//! API key by the login screen, so neither is accepted as a flag.

use std::path::PathBuf;

use clap::{Parser, ValueEnum};

/// Which Jadeite theme paints the screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ThemeKind {
    /// The primary dark theme.
    Dark,
    /// The light theme, for light terminals.
    Light,
}
use rhema_coding_agent::CodingAgentConfig;

/// `rhema-tui` — the full-screen terminal UI for the rhema coding agent.
#[derive(Debug, Parser)]
#[command(
    name = "rhema-tui",
    version,
    about = "Full-screen terminal UI for the rhema coding agent"
)]
pub struct Args {
    /// Workspace root for tools.
    #[arg(long, default_value = ".")]
    pub workspace: PathBuf,

    /// Resume a previously saved session file.
    #[arg(long)]
    pub resume: Option<PathBuf>,

    /// Checkpoint the session after every turn.
    #[arg(long)]
    pub save: Option<PathBuf>,

    /// Max steps per turn.
    #[arg(long, default_value_t = CodingAgentConfig::DEFAULT_MAX_TURN_STEPS)]
    pub max_steps: u32,

    /// Approve all gated tool calls without asking.
    #[arg(long)]
    pub no_confirm: bool,

    /// Color theme. Dark is the design's primary; light exists for
    /// light-terminal users.
    #[arg(long, value_enum, default_value_t = ThemeKind::Dark)]
    pub theme: ThemeKind,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Args {
        Args::parse_from(std::iter::once("rhema-tui").chain(args.iter().copied()))
    }

    fn try_parse(args: &[&str]) -> Result<Args, clap::Error> {
        Args::try_parse_from(std::iter::once("rhema-tui").chain(args.iter().copied()))
    }

    #[test]
    fn defaults_match_the_spec() {
        let args = parse(&[]);
        assert_eq!(args.workspace, PathBuf::from("."));
        assert_eq!(args.resume, None);
        assert_eq!(args.save, None);
        assert_eq!(args.max_steps, CodingAgentConfig::DEFAULT_MAX_TURN_STEPS);
        assert!(!args.no_confirm);
    }

    #[test]
    fn flags_parse() {
        let args = parse(&[
            "--workspace",
            "/tmp/ws",
            "--resume",
            "s.json",
            "--save",
            "s.json",
            "--max-steps",
            "4",
            "--no-confirm",
        ]);
        assert_eq!(args.workspace, PathBuf::from("/tmp/ws"));
        assert_eq!(args.resume, Some(PathBuf::from("s.json")));
        assert_eq!(args.save, Some(PathBuf::from("s.json")));
        assert_eq!(args.max_steps, 4);
        assert!(args.no_confirm);
    }

    #[test]
    fn no_model_or_api_key_flag_exists() {
        // The picker owns the model and the login owns the key; both flags
        // must be rejected rather than silently ignored.
        assert!(try_parse(&["--model", "x"]).is_err());
        assert!(try_parse(&["--api-key", "x"]).is_err());
        assert!(try_parse(&["-m", "x"]).is_err());
    }
}
