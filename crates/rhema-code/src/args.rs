//! The command-line surface, parsed with clap's derive API. Deliberately
//! narrower than the rhema CLI: the model is owned by the picker and
//! credentials by the login screen, so neither is accepted as a flag.

use std::path::PathBuf;

use clap::{Parser, ValueEnum};
use rhema_coding_agent::CodingAgentConfig;

/// Which Jadeite theme paints the screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ThemeKind {
    /// The primary dark theme.
    Dark,
    /// The light theme, for light terminals.
    Light,
}

/// Which model backend serves turns. Defaults to the stored choice, then
/// google.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ProviderChoice {
    /// Google Gemini, by API key.
    Google,
    /// A llama.cpp server by URL.
    LlamaCpp,
    /// OpenRouter, by API key.
    OpenRouter,
    /// Anthropic, by API key.
    #[value(name = "anthropic")]
    Anthropic,
    /// OpenAI, by API key.
    #[value(name = "openai")]
    OpenAi,
}

impl ProviderChoice {
    /// Parses a stored config value; unknown spellings fall back to
    /// google.
    pub fn from_stored(value: Option<&str>) -> Self {
        match value {
            Some("llama-cpp") | Some("llama_cpp") | Some("llamacpp") => Self::LlamaCpp,
            Some("openrouter") => Self::OpenRouter,
            Some("anthropic") => Self::Anthropic,
            Some("openai") => Self::OpenAi,
            _ => Self::Google,
        }
    }
}

/// `rhema` — the rhema frontend: a full-screen terminal UI when attached
/// to a terminal, a pipeable one-shot runner otherwise.
#[derive(Debug, Parser)]
#[command(
    name = "rhema",
    version,
    about = "Full-screen terminal UI for the rhema coding agent"
)]
pub struct Args {
    /// Run one prompt headless — no full-screen UI, plain text on stdout,
    /// tool events on stderr, exit 0 on success. `-` reads the prompt from
    /// stdin, so pipelines work: `git diff | rhema --print -`.
    #[arg(short = 'p', long)]
    pub print: Option<String>,

    /// Model name. Defaults to the stored choice, then the backend's
    /// default.
    #[arg(short = 'm', long)]
    pub model: Option<String>,

    /// Which config profile to use (e.g. work, personal). Without it the
    /// session starts on the default profile, or the first one holding
    /// usable credentials.
    #[arg(long)]
    pub profile: Option<String>,

    /// Model backend for turns. Defaults to the stored choice, then
    /// google.
    #[arg(long, value_enum)]
    pub provider: Option<ProviderChoice>,

    /// llama.cpp server base URL (provider llama-cpp). Defaults to the
    /// stored URL, then http://localhost:8080.
    #[arg(long)]
    pub llama_url: Option<String>,

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

    /// Color theme. Dark is the design's primary; light exists for
    /// light-terminal users.
    #[arg(long, value_enum, default_value_t = ThemeKind::Dark)]
    pub theme: ThemeKind,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Args {
        Args::parse_from(std::iter::once("rhema").chain(args.iter().copied()))
    }

    fn try_parse(args: &[&str]) -> Result<Args, clap::Error> {
        Args::try_parse_from(std::iter::once("rhema").chain(args.iter().copied()))
    }

    #[test]
    fn defaults_match_the_spec() {
        let args = parse(&[]);
        assert_eq!(args.provider, None);
        assert_eq!(args.llama_url, None);
        assert_eq!(args.workspace, PathBuf::from("."));
        assert_eq!(args.resume, None);
        assert_eq!(args.save, None);
        assert_eq!(args.max_steps, CodingAgentConfig::DEFAULT_MAX_TURN_STEPS);
        assert_eq!(args.theme, ThemeKind::Dark);
    }

    #[test]
    fn flags_parse() {
        let args = parse(&[
            "--provider",
            "llama-cpp",
            "--llama-url",
            "http://box:8080",
            "--workspace",
            "/tmp/ws",
            "--resume",
            "s.json",
            "--save",
            "s.json",
            "--max-steps",
            "4",
            "--theme",
            "light",
        ]);
        assert_eq!(args.provider, Some(ProviderChoice::LlamaCpp));
        assert_eq!(args.llama_url.as_deref(), Some("http://box:8080"));
        assert_eq!(args.workspace, PathBuf::from("/tmp/ws"));
        assert_eq!(args.resume, Some(PathBuf::from("s.json")));
        assert_eq!(args.save, Some(PathBuf::from("s.json")));
        assert_eq!(args.max_steps, 4);
        assert_eq!(args.theme, ThemeKind::Light);
    }

    #[test]
    fn stored_provider_spellings_map() {
        assert_eq!(
            ProviderChoice::from_stored(Some("llama_cpp")),
            ProviderChoice::LlamaCpp
        );
        assert_eq!(
            ProviderChoice::from_stored(Some("google")),
            ProviderChoice::Google
        );
        assert_eq!(
            ProviderChoice::from_stored(Some("anthropic")),
            ProviderChoice::Anthropic
        );
        assert_eq!(
            ProviderChoice::from_stored(Some("openai")),
            ProviderChoice::OpenAi
        );
        assert_eq!(ProviderChoice::from_stored(None), ProviderChoice::Google);
        assert_eq!(
            ProviderChoice::from_stored(Some("nonsense")),
            ProviderChoice::Google
        );
    }

    #[test]
    fn the_new_providers_parse_as_cli_values_matching_the_stored_names() {
        let args = parse(&["--provider", "anthropic"]);
        assert_eq!(args.provider, Some(ProviderChoice::Anthropic));
        let args = parse(&["--provider", "openai"]);
        assert_eq!(args.provider, Some(ProviderChoice::OpenAi));
    }

    #[test]
    fn no_api_key_flag_exists() {
        // The login owns the key: interactive or headless, it is never a
        // flag (shells keep history).
        assert!(try_parse(&["--api-key", "x"]).is_err());
    }

    #[test]
    fn the_print_flag_parses_forms() {
        let args = parse(&["--print", "fix the tests"]);
        assert_eq!(args.print.as_deref(), Some("fix the tests"));
        let args = parse(&["-p", "-"]);
        assert_eq!(args.print.as_deref(), Some("-"));
    }
}
