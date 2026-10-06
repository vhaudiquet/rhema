//! The command-line surface, parsed with clap's derive API.

use std::path::PathBuf;

use clap::Parser;
use rhema_coding_agent::CodingAgentConfig;

/// `rhema` — terminal REPL for the rhema coding agent. The full-screen TUI lives in `rhema-tui`.
#[derive(Debug, Parser)]
#[command(
    name = "rhema",
    version,
    about = "Terminal REPL for the rhema coding agent"
)]
pub struct Args {
    /// One-shot: run a single turn with this prompt and exit. Omit for the
    /// interactive REPL.
    pub prompt: Option<String>,

    /// Model name. Defaults to the stored choice in the rhema config, then
    /// to `gemini-3.8-flash`.
    #[arg(short = 'm', long)]
    pub model: Option<String>,

    /// Workspace root for tools.
    #[arg(short = 'w', long, default_value = ".")]
    pub workspace: PathBuf,

    /// Resume a previously saved session file.
    #[arg(long)]
    pub resume: Option<PathBuf>,

    /// Save the session after every turn (checkpoint per turn).
    #[arg(long)]
    pub save: Option<PathBuf>,

    /// Approve all gated tool calls without asking.
    #[arg(long)]
    pub no_confirm: bool,

    /// Max steps per turn.
    #[arg(long, default_value_t = CodingAgentConfig::DEFAULT_MAX_TURN_STEPS)]
    pub max_steps: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Args {
        Args::parse_from(std::iter::once("rhema").chain(args.iter().copied()))
    }

    #[test]
    fn defaults_match_the_spec() {
        let args = parse(&[]);
        assert_eq!(args.model, None);
        assert_eq!(args.workspace, PathBuf::from("."));
        assert_eq!(args.resume, None);
        assert_eq!(args.save, None);
        assert!(!args.no_confirm);
        assert_eq!(args.max_steps, CodingAgentConfig::DEFAULT_MAX_TURN_STEPS);
        assert_eq!(args.prompt, None);
    }

    #[test]
    fn a_positional_prompt_selects_one_shot_mode() {
        assert_eq!(parse(&["fix the test"]).prompt.unwrap(), "fix the test");
    }

    #[test]
    fn flags_parse_in_short_and_long_forms() {
        let args = parse(&[
            "-m",
            "gemini-2.5-pro",
            "-w",
            "/tmp/ws",
            "--resume",
            "s.json",
            "--save",
            "s.json",
            "--no-confirm",
            "--max-steps",
            "4",
        ]);
        assert_eq!(args.model, Some("gemini-2.5-pro".into()));
        assert_eq!(args.workspace, PathBuf::from("/tmp/ws"));
        assert_eq!(args.resume, Some(PathBuf::from("s.json")));
        assert_eq!(args.save, Some(PathBuf::from("s.json")));
        assert!(args.no_confirm);
        assert_eq!(args.max_steps, 4);
    }

    #[test]
    fn the_step_budget_defaults_to_the_agent_constant() {
        assert_eq!(CodingAgentConfig::DEFAULT_MAX_TURN_STEPS, 16);
    }
}
