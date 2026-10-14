//! The command-line surface, parsed with clap's derive API. Deliberately
//! narrow: the model is owned by the picker and credentials by the login
//! screen, so neither is accepted as a flag, and the provider is a
//! stored-profile or `RHEMA_PROVIDER` decision, not a per-invocation
//! one. Subcommands switch the frontend: `run` is the pipeable one-shot,
//! `acp` serves the Agent Client Protocol; its flags mirror the run
//! subset minus the workspace (the client names it per session).

use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};
use rhema_coding_agent::CodingAgentConfig;

/// Which Jadeite theme paints the screen. Parsed from the stored config
/// (`theme: dark|light`), not a flag — a theme is a machine choice, not
/// a per-invocation one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ThemeKind {
    /// The primary dark theme.
    Dark,
    /// The light theme, for light terminals.
    Light,
}

/// Which model backend serves turns, by the stored config's provider
/// names.
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
    /// DeepSeek, by API key (their OpenAI-compatible endpoint).
    #[value(name = "deepseek")]
    DeepSeek,
    /// Z.ai GLM Coding Plan, by API key (their Anthropic-compatible
    /// endpoint).
    #[value(name = "zai")]
    Zai,
    /// GitHub Copilot, by GitHub OAuth token (the device flow's output).
    #[value(name = "copilot")]
    Copilot,
}

impl ProviderChoice {
    /// Parses a stored config value; `None` for no value and unknown
    /// spellings alike — the caller decides whether that means the
    /// login flow or an error naming the stored value.
    pub fn from_stored(value: Option<&str>) -> Option<Self> {
        match value {
            Some("google") => Some(Self::Google),
            Some("llama-cpp") | Some("llama_cpp") | Some("llamacpp") => Some(Self::LlamaCpp),
            Some("openrouter") => Some(Self::OpenRouter),
            Some("anthropic") => Some(Self::Anthropic),
            Some("openai") => Some(Self::OpenAi),
            Some("deepseek") => Some(Self::DeepSeek),
            Some("zai") => Some(Self::Zai),
            Some("copilot") => Some(Self::Copilot),
            _ => None,
        }
    }

    /// The config value this choice persists as — the inverse of
    /// [`Self::from_stored`] for the canonical spellings.
    pub fn stored_name(self) -> &'static str {
        match self {
            Self::Google => "google",
            Self::LlamaCpp => "llama-cpp",
            Self::OpenRouter => "openrouter",
            Self::Anthropic => "anthropic",
            Self::OpenAi => "openai",
            Self::DeepSeek => "deepseek",
            Self::Zai => "zai",
            Self::Copilot => "copilot",
        }
    }
}

/// `rhema` — the rhema frontend: a full-screen terminal UI when attached
/// to a terminal, and one-shot or ACP serving through a subcommand.
#[derive(Debug, Parser)]
#[command(
    name = "rhema",
    // The baked label, not the bare crate version: `--version` answers
    // the same question the banner does — which tree is this binary.
    version = crate::VERSION_LABEL,
    about = "Full-screen terminal UI for the rhema coding agent"
)]
pub struct Args {
    /// Which config profile to use (e.g. work, personal). Without it the
    /// session starts on the default profile, or the first one holding
    /// usable credentials.
    #[arg(long)]
    pub profile: Option<String>,

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

    /// How much reasoning the model should do. Absent, the profile's
    /// setting stands, and absent there too, the provider's default.
    #[arg(long)]
    pub thinking: Option<ThinkingKind>,

    /// A subcommand switches the frontend; without one, rhema runs the
    /// terminal UI.
    #[command(subcommand)]
    pub command: Option<Command>,
}

/// The alternate frontends.
#[derive(Debug, Clone, PartialEq, Eq, Subcommand)]
pub enum Command {
    /// Run one prompt headless — no full-screen UI, plain text on
    /// stdout, tool events on stderr, exit 0 on success.
    Run(RunArgs),
    /// Serve the Agent Client Protocol over stdio: the coding agent as
    /// an editor-launched subprocess (Zed, Neovim, JetBrains, Emacs).
    Acp(AcpArgs),
    /// Edit, restore, or print the system prompt. No model, profile,
    /// or credential involved — these shape what turns would send.
    System {
        /// What to do with the system prompt.
        #[command(subcommand)]
        action: SystemCommand,
    },
}

/// What `rhema system` does with the prompt. The edited file is the
/// config's `SYSTEM.md`, a Jinja template rendered per turn over the
/// workspace, the discovered skills, and the tool specs.
#[derive(Debug, Clone, PartialEq, Eq, Subcommand)]
pub enum SystemCommand {
    /// Open `$EDITOR` (then `$VISUAL`) on the config's SYSTEM.md,
    /// seeding it with the built-in default first when the file does
    /// not exist yet.
    Edit,
    /// Overwrite the config's SYSTEM.md with the built-in default.
    Restore,
    /// Print the prompt the next turn in `--workspace` would send: the
    /// config's SYSTEM.md when present, the built-in default
    /// otherwise, with the workspace, the discovered skills, and the
    /// tools rendered in.
    Show,
}

/// Flags of `rhema run`, the headless one-shot built for pipes.
#[derive(Debug, Clone, PartialEq, Eq, clap::Args)]
pub struct RunArgs {
    /// The prompt. `-` reads it from stdin, so pipelines work:
    /// `git diff | rhema run -`. Omitted, a piped stdin is the prompt;
    /// with a terminal stdin and no prompt rhema errors.
    pub prompt: Option<String>,

    /// Which config profile to use (e.g. work, personal). Without it the
    /// session starts on the default profile, or the first one holding
    /// usable credentials.
    #[arg(long)]
    pub profile: Option<String>,

    /// Model name. Defaults to the stored choice, then the backend's
    /// default.
    #[arg(short = 'm', long)]
    pub model: Option<String>,

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

    /// How much reasoning the model should do. Absent, the profile's
    /// setting stands, and absent there too, the provider's default.
    #[arg(long)]
    pub thinking: Option<ThinkingKind>,
}

/// Flags of `rhema acp`. The workspace comes from the client's
/// `session/new` (`cwd`), not a flag — one process serves every
/// workspace the client opens.
#[derive(Debug, Clone, PartialEq, Eq, clap::Args)]
pub struct AcpArgs {
    /// Which config profile to use (e.g. work, personal). Without it
    /// the default profile stands.
    #[arg(long)]
    pub profile: Option<String>,

    /// Model name. Defaults to the stored choice, then the backend's
    /// default.
    #[arg(short = 'm', long)]
    pub model: Option<String>,

    /// Max steps per turn.
    #[arg(long, default_value_t = CodingAgentConfig::DEFAULT_MAX_TURN_STEPS)]
    pub max_steps: u32,

    /// How much reasoning the model should do. Absent, the profile's
    /// setting stands, and absent there too, the provider's default.
    #[arg(long)]
    pub thinking: Option<ThinkingKind>,
}

/// Reasoning effort levels for `--thinking`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum ThinkingKind {
    /// Light reasoning.
    Low,
    /// The middle setting.
    Medium,
    /// Maximum reasoning effort.
    High,
}

impl From<ThinkingKind> for rhema_core::request::ThinkingEffort {
    fn from(kind: ThinkingKind) -> Self {
        match kind {
            ThinkingKind::Low => Self::Low,
            ThinkingKind::Medium => Self::Medium,
            ThinkingKind::High => Self::High,
        }
    }
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
        assert_eq!(args.profile, None);
        assert_eq!(args.workspace, PathBuf::from("."));
        assert_eq!(args.resume, None);
        assert_eq!(args.save, None);
        assert_eq!(args.max_steps, CodingAgentConfig::DEFAULT_MAX_TURN_STEPS);
        assert_eq!(args.thinking, None);
    }

    #[test]
    fn flags_parse() {
        let args = parse(&[
            "--profile",
            "work",
            "--workspace",
            "/tmp/ws",
            "--resume",
            "s.json",
            "--save",
            "s.json",
            "--max-steps",
            "4",
            "--thinking",
            "low",
        ]);
        assert_eq!(args.profile.as_deref(), Some("work"));
        assert_eq!(args.workspace, PathBuf::from("/tmp/ws"));
        assert_eq!(args.resume, Some(PathBuf::from("s.json")));
        assert_eq!(args.save, Some(PathBuf::from("s.json")));
        assert_eq!(args.max_steps, 4);
        assert_eq!(args.thinking, Some(ThinkingKind::Low));
    }

    #[test]
    fn stored_provider_spellings_map() {
        assert_eq!(
            ProviderChoice::from_stored(Some("llama_cpp")),
            Some(ProviderChoice::LlamaCpp)
        );
        assert_eq!(
            ProviderChoice::from_stored(Some("google")),
            Some(ProviderChoice::Google)
        );
        assert_eq!(
            ProviderChoice::from_stored(Some("anthropic")),
            Some(ProviderChoice::Anthropic)
        );
        assert_eq!(
            ProviderChoice::from_stored(Some("openai")),
            Some(ProviderChoice::OpenAi)
        );
        // Nothing stored and a spelling no provider answers to both read
        // as unconfigured: the caller errors or opens the login flow, it
        // never silently serves google.
        assert_eq!(ProviderChoice::from_stored(None), None);
        assert_eq!(ProviderChoice::from_stored(Some("nonsense")), None);
    }

    #[test]
    fn the_new_providers_parse_as_cli_values_matching_the_stored_names() {
        // The provider is a stored choice (or RHEMA_PROVIDER), not a
        // flag: the spellings round-trip through from_stored.
        assert_eq!(
            ProviderChoice::from_stored(Some("anthropic")),
            Some(ProviderChoice::Anthropic)
        );
        assert_eq!(
            ProviderChoice::from_stored(Some("openai")),
            Some(ProviderChoice::OpenAi)
        );
        assert_eq!(
            ProviderChoice::from_stored(Some("deepseek")),
            Some(ProviderChoice::DeepSeek)
        );
        assert_eq!(
            ProviderChoice::from_stored(Some("zai")),
            Some(ProviderChoice::Zai)
        );
        assert_eq!(
            ProviderChoice::from_stored(Some("copilot")),
            Some(ProviderChoice::Copilot)
        );
        assert_eq!(ProviderChoice::Copilot.stored_name(), "copilot");
        assert_eq!(ProviderChoice::Zai.stored_name(), "zai");
    }

    #[test]
    fn no_provider_or_llama_flag_exists() {
        // The provider is a profile decision, the llama.cpp server a
        // stored URL (or LLAMA_CPP_URL) — neither is a flag.
        assert!(try_parse(&["--provider", "anthropic"]).is_err());
        assert!(try_parse(&["--llama-url", "http://box:8080"]).is_err());
        // Same for the machine-owned look and feel: config.yaml owns it.
        assert!(try_parse(&["--theme", "light"]).is_err());
        assert!(try_parse(&["--icons", "nerd"]).is_err());
    }

    #[test]
    fn no_api_key_flag_exists() {
        // The login owns the key: interactive or headless, it is never a
        // flag (shells keep history).
        assert!(try_parse(&["--api-key", "x"]).is_err());
    }

    #[test]
    fn the_run_subcommand_takes_the_prompt() {
        let args = parse(&["run", "fix the tests"]);
        match args.command {
            Some(Command::Run(run)) => {
                assert_eq!(run.prompt.as_deref(), Some("fix the tests"));
                assert_eq!(run.workspace, PathBuf::from("."));
            }
            other => panic!("expected the run subcommand, got {other:?}"),
        }
        // `-` reads the prompt from stdin, so pipelines work.
        let args = parse(&["run", "-"]);
        match args.command {
            Some(Command::Run(run)) => assert_eq!(run.prompt.as_deref(), Some("-")),
            other => panic!("expected the run subcommand, got {other:?}"),
        }
        // The prompt is optional at parse time; with a terminal stdin and
        // nothing piped the runner errors instead of opening a REPL.
        assert!(matches!(parse(&["run"]).command, Some(Command::Run(_))));
    }

    #[test]
    fn the_run_subcommand_takes_the_headless_flags() {
        let args = parse(&[
            "run",
            "--profile",
            "work",
            "-m",
            "m1",
            "--workspace",
            "/tmp/ws",
            "--resume",
            "s.json",
            "--save",
            "s.json",
            "--max-steps",
            "4",
            "--thinking",
            "low",
        ]);
        match args.command {
            Some(Command::Run(run)) => {
                assert_eq!(run.prompt, None);
                assert_eq!(run.profile.as_deref(), Some("work"));
                assert_eq!(run.model.as_deref(), Some("m1"));
                assert_eq!(run.workspace, PathBuf::from("/tmp/ws"));
                assert_eq!(run.resume, Some(PathBuf::from("s.json")));
                assert_eq!(run.save, Some(PathBuf::from("s.json")));
                assert_eq!(run.max_steps, 4);
                assert_eq!(run.thinking, Some(ThinkingKind::Low));
            }
            other => panic!("expected the run subcommand, got {other:?}"),
        }
    }

    #[test]
    fn the_acp_subcommand_takes_its_own_flags() {
        let args = Args::parse_from(["rhema", "acp"]);
        assert_eq!(
            args.command,
            Some(Command::Acp(AcpArgs {
                profile: None,
                model: None,
                max_steps: CodingAgentConfig::DEFAULT_MAX_TURN_STEPS,
                thinking: None,
            }))
        );

        let args = Args::parse_from([
            "rhema",
            "acp",
            "--profile",
            "work",
            "-m",
            "claude-sonnet-4-5",
            "--max-steps",
            "16",
            "--thinking",
            "high",
        ]);
        match args.command {
            Some(Command::Acp(acp)) => {
                assert_eq!(acp.profile.as_deref(), Some("work"));
                assert_eq!(acp.model.as_deref(), Some("claude-sonnet-4-5"));
                assert_eq!(acp.max_steps, 16);
                assert_eq!(acp.thinking, Some(ThinkingKind::High));
            }
            other => panic!("expected the acp subcommand, got {other:?}"),
        }
    }

    #[test]
    fn the_acp_flags_do_not_leak_into_the_default_frontend() {
        // The workspace belongs to session/new under ACP: no --workspace
        // before the subcommand name, where it would be ambiguous with
        // the TUI's.
        assert!(try_parse(&["acp", "--workspace", "/tmp"]).is_err());
        // And the model flag belongs to the subcommands only: the TUI's
        // model is the picker's or the stored choice's to make.
        assert!(try_parse(&["--print", "hi"]).is_err());
        assert!(try_parse(&["-m", "m"]).is_err());
    }

    #[test]
    fn the_system_subcommand_takes_an_action() {
        let args = parse(&["system", "show"]);
        assert_eq!(
            args.command,
            Some(Command::System {
                action: SystemCommand::Show
            })
        );
        let args = parse(&["system", "edit"]);
        assert_eq!(
            args.command,
            Some(Command::System {
                action: SystemCommand::Edit
            })
        );
        let args = parse(&["system", "restore"]);
        assert_eq!(
            args.command,
            Some(Command::System {
                action: SystemCommand::Restore
            })
        );
        // An action is required: bare `rhema system` is a usage error,
        // not a default.
        assert!(try_parse(&["system"]).is_err());
    }
}
