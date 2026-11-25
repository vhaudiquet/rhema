//! The command-line surface, parsed with clap's derive API. Deliberately
//! narrow: the model is owned by the picker and credentials by the login
//! screen, so neither is accepted as a flag, and the provider is a
//! stored-profile or `RHEMA_PROVIDER` decision, not a per-invocation
//! one. Subcommands switch the frontend: `run` is the pipeable one-shot,
//! `acp` serves the Agent Client Protocol; its flags mirror the run
//! subset minus the workspace (the client names it per session).

use std::ffi::OsString;
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};

use clap::{Parser, Subcommand, ValueEnum};
use rhema_host_agent::HostAgentConfig;

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
    /// OpenCode Console or Go, by Console API key.
    #[value(name = "opencode")]
    OpenCode,
    /// GitHub Copilot, by GitHub OAuth token (the device flow's output).
    #[value(name = "copilot")]
    Copilot,
    /// A ChatGPT subscription, by the browser sign-in's credential.
    #[value(name = "chatgpt")]
    ChatGpt,
    /// Mistral's hosted API, by API key.
    #[value(name = "mistral")]
    Mistral,
    /// A Mistral Vibe subscription, by the browser sign-in's API key.
    #[value(name = "mistral-vibe")]
    MistralVibe,
}

impl ProviderChoice {
    /// Parses a stored config value; `None` for no value and unknown
    /// spellings alike, the caller decides whether that means the
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
            Some("opencode") => Some(Self::OpenCode),
            Some("copilot") => Some(Self::Copilot),
            Some("chatgpt") => Some(Self::ChatGpt),
            Some("mistral") => Some(Self::Mistral),
            Some("mistral-vibe") => Some(Self::MistralVibe),
            _ => None,
        }
    }

    /// The config value this choice persists as, the inverse of
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
            Self::OpenCode => "opencode",
            Self::Copilot => "copilot",
            Self::ChatGpt => "chatgpt",
            Self::Mistral => "mistral",
            Self::MistralVibe => "mistral-vibe",
        }
    }
}

/// Where `bash` runs, by the stored config's sandbox kind names, the
/// [`ProviderChoice`] pattern applied to machines. The choice is a
/// stored-profile or `RHEMA_SANDBOX` decision, never a per-invocation
/// flag; the TUI picker and the availability probing are the next
/// phase of the sandbox design (`specs/rhema-sandbox.md` §5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum SandboxChoice {
    /// This host's processes rooted at the workspace, today's path,
    /// and the fallback for anything unset or unparseable.
    Local,
    /// A bind-mounted container, driven through the `docker` CLI.
    Docker,
    /// An LXD instance, container or `--vm`, one driver and a launch
    /// flag, driven through the `lxc` CLI.
    Lxd,
    /// A microVM per command through the `ecrou` CLI, the workspace
    /// shared in at its own path.
    Ecrou,
    /// A remote box over ssh, nothing installed on the far end.
    Ssh,
    /// A daemon speaking the sandbox protocol over HTTPS
    /// (`rhema-sandboxd`).
    Http,
}

impl SandboxChoice {
    /// Parses a stored config value; `None` for no value and unknown
    /// spellings alike, the caller decides whether that means the
    /// zero-flag fast path or a startup error naming the stored value.
    pub fn from_stored(value: Option<&str>) -> Option<Self> {
        match value {
            Some("local") => Some(Self::Local),
            Some("docker") => Some(Self::Docker),
            Some("lxd") => Some(Self::Lxd),
            Some("ecrou") => Some(Self::Ecrou),
            Some("ssh") => Some(Self::Ssh),
            Some("http") => Some(Self::Http),
            _ => None,
        }
    }

    /// The config value this choice persists as, the inverse of
    /// [`Self::from_stored`] for the canonical spellings.
    pub fn stored_name(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Docker => "docker",
            Self::Lxd => "lxd",
            Self::Ecrou => "ecrou",
            Self::Ssh => "ssh",
            Self::Http => "http",
        }
    }
}

/// `rhema`, the rhema frontend: a full-screen terminal UI when attached
/// to a terminal, and one-shot or ACP serving through a subcommand.
#[derive(Debug, Parser)]
#[command(
    name = "rhema",
    // The baked label, not the bare crate version: `--version` answers
    // the same question the banner does, which tree is this binary.
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
    #[arg(long, default_value_t = HostAgentConfig::DEFAULT_MAX_TURN_STEPS)]
    pub max_steps: NonZeroU32,

    /// Send the whole history every step: skip the bounded request
    /// view that summarizes older messages into lines the `zoom` tool
    /// can open. The view is on by default and pass-through below its
    /// budget.
    #[arg(long)]
    pub no_transcript: bool,

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
    /// Run one prompt headless, no full-screen UI, plain text on
    /// stdout, tool events on stderr, exit 0 on success, 2 when the
    /// model backend failed (a rate limit, a dead endpoint, a wrapper
    /// retries on its own schedule), 1 otherwise.
    Run(RunArgs),
    /// Serve the Agent Client Protocol over stdio: the coding agent as
    /// an editor-launched subprocess (Zed, Neovim, JetBrains, Emacs).
    Acp(AcpArgs),
    /// Edit, restore, or print the system prompt. No model, profile,
    /// or credential involved, these shape what turns would send.
    System {
        /// What to do with the system prompt.
        #[command(subcommand)]
        action: SystemCommand,
    },
    /// Run the first-run onboarding wizard again: profile, provider,
    /// model, colors, skills. The terminal UI with the wizard forced
    /// on, whatever the config's marker says.
    Onboard,
    /// Install the channel's newest release and exit: the same check
    /// and the same installer the settings' Update page runs, with
    /// plain lines on stdout. Exit 0 when this build is the newest or
    /// the install landed, 1 when the check or the install failed.
    Update,
    /// The MCP meta surface (`mcp servers | tools | schema | call`)
    /// plus the config verbs (`mcp add | list | remove`, which write a
    /// profile's `mcp_servers` and so work outside any wired shell),
    /// and the pass-through that makes the agent shell's generated
    /// functions work: `rhema __server <name> …`. Everything after
    /// the subcommand goes to the MCP CLI verbatim; the bridge verbs
    /// read the env the init file set and exit 4 outside a wired
    /// shell.
    Mcp {
        /// The CLI's own argv, passed through untouched.
        #[arg(allow_hyphen_values = true, trailing_var_arg = true)]
        args: Vec<std::ffi::OsString>,
    },
    /// Prints the runtime kit version. The installer checks it after
    /// a copy and the host compares it before using an install.
    #[command(name = "__kit-version")]
    KitVersion,
    /// Prints the static init file for a kit: the helper's `__init`
    /// mode, called once per install. `--stub` names one function and
    /// repeats: `job=RHEMA_JOBS_BIN:__jobs`.
    #[command(name = "__init")]
    Init {
        /// One stub, `name=bin_var:args`, args whitespace-separated.
        #[arg(long = "stub", value_name = "NAME=BIN_VAR:ARGS")]
        stub: Vec<String>,
    },
    /// The pass-through that makes the agent shell's `see` macro work:
    /// `rhema __vision …` execs this binary in the vision CLI, which
    /// stages the piped bytes (or the named files) into the session's
    /// staging directory. Outside a wired shell it exits 4 with a hint.
    #[command(name = "__vision")]
    Vision {
        /// The CLI's own argv, passed through untouched.
        #[arg(allow_hyphen_values = true, trailing_var_arg = true)]
        args: Vec<std::ffi::OsString>,
    },
    /// The pass-through that makes the agent shell's `job` macro work:
    /// `rhema __jobs …` owns the background-job verbs, which delegate to
    /// the backend CLI behind one shim (see [`rhema_bash::jobs`]).
    /// Outside a wired shell the verbs that need a session exit 4.
    #[command(name = "__jobs")]
    Jobs {
        /// The CLI's own argv, passed through untouched.
        #[arg(allow_hyphen_values = true, trailing_var_arg = true)]
        args: Vec<std::ffi::OsString>,
    },
    /// The pass-through that makes the agent shell's `search` macro
    /// work: `rhema __search "query"` execs this binary in the search
    /// CLI, which runs the session profile's provider chain and prints
    /// the markdown. Outside a wired shell it exits 4 with a hint.
    #[command(name = "__search")]
    Search {
        /// The CLI's own argv, passed through untouched.
        #[arg(allow_hyphen_values = true, trailing_var_arg = true)]
        args: Vec<std::ffi::OsString>,
    },
    /// The pass-through that makes the agent shell's `decide` macro
    /// work: `rhema __decision …` execs this binary in the decision
    /// CLI, which asks the session profile's decision provider and
    /// prints the answers. Outside a wired shell it exits 4.
    #[command(name = "__decision")]
    Decision {
        /// The CLI's own argv, passed through untouched.
        #[arg(allow_hyphen_values = true, trailing_var_arg = true)]
        args: Vec<std::ffi::OsString>,
    },
    /// The catch-all the init file's stubs land in: `rhema __server
    /// <name> …` execs this binary with `__server` as the first
    /// token, and any other unrecognized subcommand follows the same
    /// road, the MCP CLI answers for the modes it knows and exits 2
    /// for the ones it does not, preserving the old clap shape's
    /// error exit.
    #[command(external_subcommand)]
    External(Vec<OsString>),
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
    #[arg(long, default_value_t = HostAgentConfig::DEFAULT_MAX_TURN_STEPS)]
    pub max_steps: NonZeroU32,

    /// Send the whole history every step: skip the bounded request
    /// view that summarizes older messages into lines the `zoom` tool
    /// can open. The view is on by default and pass-through below its
    /// budget.
    #[arg(long)]
    pub no_transcript: bool,

    /// How much reasoning the model should do. Absent, the profile's
    /// setting stands, and absent there too, the provider's default.
    #[arg(long)]
    pub thinking: Option<ThinkingKind>,
}

/// Flags of `rhema acp`. The workspace comes from the client's
/// `session/new` (`cwd`), not a flag, one process serves every
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
    #[arg(long, default_value_t = HostAgentConfig::DEFAULT_MAX_TURN_STEPS)]
    pub max_steps: NonZeroU32,

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

/// The path as the session sees it: canonicalized, falling back to the
/// input when the filesystem says no, and on windows with the
/// `\\?\` verbatim prefix canonicalize adds stripped, so the footer's
/// home trim, the window title, and the model all read the spelling a
/// human typed.
pub(crate) fn canonical(path: &Path) -> PathBuf {
    let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    strip_verbatim(canonical)
}

/// Drops the verbatim prefix a windows canonicalize produces:
/// `\\?\C:\...` answers as `C:\...`, and `\\?\UNC\srv\share` as
/// `\\srv\share`. Elsewhere, the path itself.
#[cfg(windows)]
fn strip_verbatim(path: PathBuf) -> PathBuf {
    let text = path.as_os_str().to_string_lossy();
    if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        return PathBuf::from(format!(r"\\{rest}"));
    }
    match text.strip_prefix(r"\\?\") {
        Some(rest) => PathBuf::from(rest),
        None => path,
    }
}

#[cfg(not(windows))]
fn strip_verbatim(path: PathBuf) -> PathBuf {
    path
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
        assert_eq!(args.max_steps, HostAgentConfig::DEFAULT_MAX_TURN_STEPS);
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
        assert_eq!(args.max_steps, NonZeroU32::new(4).unwrap());
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
        assert_eq!(
            ProviderChoice::from_stored(Some("chatgpt")),
            Some(ProviderChoice::ChatGpt)
        );
        assert_eq!(ProviderChoice::ChatGpt.stored_name(), "chatgpt");
        assert_eq!(
            ProviderChoice::from_stored(Some("mistral")),
            Some(ProviderChoice::Mistral)
        );
        assert_eq!(ProviderChoice::Mistral.stored_name(), "mistral");
        assert_eq!(
            ProviderChoice::from_stored(Some("mistral-vibe")),
            Some(ProviderChoice::MistralVibe)
        );
        assert_eq!(ProviderChoice::MistralVibe.stored_name(), "mistral-vibe");
        assert_eq!(ProviderChoice::Zai.stored_name(), "zai");
    }

    #[test]
    fn sandbox_spellings_round_trip_through_the_stored_names() {
        // The sandbox is a stored choice (or RHEMA_SANDBOX), not a
        // flag: every canonical spelling parses, and every choice
        // persists back as exactly the spelling that parses to it.
        for (name, expected) in [
            ("local", SandboxChoice::Local),
            ("docker", SandboxChoice::Docker),
            ("lxd", SandboxChoice::Lxd),
            ("ssh", SandboxChoice::Ssh),
            ("http", SandboxChoice::Http),
            ("ecrou", SandboxChoice::Ecrou),
        ] {
            assert_eq!(SandboxChoice::from_stored(Some(name)), Some(expected));
            assert_eq!(expected.stored_name(), name);
        }
        // Nothing stored and a spelling no sandbox answers to both read
        // as unconfigured: the caller falls back to the local fast path,
        // it never silently stands a container up.
        assert_eq!(SandboxChoice::from_stored(None), None);
        assert_eq!(SandboxChoice::from_stored(Some("nonsense")), None);
    }

    #[test]
    fn no_sandbox_flag_exists() {
        // The sandbox is a profile decision like the provider: no
        // per-invocation flag shadows it.
        assert!(try_parse(&["--sandbox", "docker"]).is_err());
        assert!(try_parse(&["run", "--sandbox", "ssh"]).is_err());
    }

    #[test]
    fn no_provider_or_llama_flag_exists() {
        // The provider is a profile decision, the llama.cpp server a
        // stored URL (or LLAMA_CPP_URL), neither is a flag.
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
                assert_eq!(run.max_steps, NonZeroU32::new(4).unwrap());
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
                max_steps: HostAgentConfig::DEFAULT_MAX_TURN_STEPS,
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
                assert_eq!(acp.max_steps, NonZeroU32::new(16).unwrap());
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

    #[test]
    fn the_onboard_subcommand_parses_and_takes_nothing() {
        let args = parse(&["onboard"]);
        assert_eq!(args.command, Some(Command::Onboard));
        // No stray flags: the wizard reuses the TUI's own.
        assert!(try_parse(&["onboard", "--profile", "work"]).is_err());
    }
}
