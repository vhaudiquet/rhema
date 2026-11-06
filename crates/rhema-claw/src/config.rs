//! The claw config file: where state lives, which model answers the
//! dispatches, and the credentials the surface answers to.
//!
//! YAML at `~/.config/rhema/claw.yaml` (`--config` overrides). The
//! file carries only what boot needs; everything the daemon
//! reconfigures at runtime — watches, calendars — lives in its tables
//! behind the HTTP doors, validated where it enters. Unresolvable
//! state directories and colliding credentials fail loudly at startup;
//! a daemon that starts with nothing to guard still serves chat.
//! Forge tokens fall back to the per-forge environment variable; the
//! model backend's keys resolve through `rhema-config` exactly as the
//! TUI's do (env first, stored second) and are validated in the
//! binary's startup, not here — the daemon library takes the built
//! provider, so tests can inject a mock.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// How often the resident loop runs a detect pass, when
/// [`ClawConfig::poll_interval_secs`] is unset.
pub const DEFAULT_POLL_INTERVAL_SECS: u64 = 60;

/// How many tool rounds one chat turn may run, when
/// [`ClawConfig::max_turn_steps`] is unset — the coding driver's own
/// budget shape and its default: deep enough that a cap is a runaway
/// guard for an agent meant to work unattended, not a work timer.
pub const DEFAULT_MAX_TURN_STEPS: u32 = 2048;

/// Where the HTTP surface listens when [`ClawConfig::listen`] is unset:
/// loopback only — the surface is for the machine's resident user.
pub const DEFAULT_LISTEN: &str = "127.0.0.1:7799";

/// Why a config operation failed.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// The config file does not exist. Claw has nothing to guess from:
    /// paths and credentials are the file's job.
    #[error("no claw config at {0}; pass --config or create one")]
    Missing(PathBuf),
    /// Reading or writing the file failed.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// The YAML file is not valid or not the expected shape.
    #[error(transparent)]
    Yaml(#[from] serde_yaml::Error),
    /// The stored-rhema config dir (`~/.config/rhema`) could not be
    /// located for the default claw.yaml path.
    #[error(transparent)]
    Stored(#[from] rhema_config::ConfigError),
    /// Neither `XDG_STATE_HOME` nor `HOME` is set, so the default SQLite
    /// path or workspace root cannot be located.
    #[error("cannot locate the state directory: neither XDG_STATE_HOME nor HOME is set")]
    NoStateHome,
    /// The agent token is the human token: boot refuses, because the
    /// agent token's safety is that it is scoped, and this copy is not.
    #[error(
        "agent_token is the same value as token; the agent token must be its own secret, scoped to the tool routes"
    )]
    SameAgentToken,
    /// The ingest credential is the same value as the human token or
    /// the agent token: boot refuses, because the ingest credential's
    /// safety is that it is write-only and the event door only — a
    /// value shared with a wider class silently widens it.
    #[error(
        "ingest_token is the same value as another credential; the ingest token must be its own secret, write-only on the event door"
    )]
    IngestTokenCollision,
    /// The HTTP surface would listen beyond loopback with no bearer
    /// token: anyone who can reach the port could run tools on this
    /// machine.
    #[error(
        "listen {listen} is not a loopback address and the surface has no \
         token: set a token, or bind a loopback address"
    )]
    OpenSurface {
        /// The listen address as configured.
        listen: String,
    },
}

/// Whether a `host:port` listen address binds beyond loopback.
/// `localhost` (any case) and any loopback IP literal — `127.x.x.x`,
/// `::1` — are loopback; an empty host (`:7799`, the bind-everything
/// shape) is not. A malformed host is not loopback either: it will
/// fail at bind time, and until then it must not read as safe.
pub fn is_loopback_listen(listen: &str) -> bool {
    let host = if listen.starts_with('[') {
        // A bracketed IPv6 literal, with or without a port.
        listen
            .split(']')
            .next()
            .and_then(|host| host.strip_prefix('['))
            .unwrap_or(listen)
    } else if listen.matches(':').count() > 1 {
        // A bare IPv6 literal carries several colons; a port on one
        // would be bracketed.
        listen
    } else {
        listen.split(':').next().unwrap_or(listen)
    };
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    host.parse::<std::net::IpAddr>()
        .is_ok_and(|ip| ip.is_loopback())
}

/// The daemon's config file. Unknown keys are rejected: a typo'd key
/// must not silently read as its default, and a daemon is the worst
/// place to learn that three weeks late.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "snake_case", deny_unknown_fields)]
pub struct ClawConfig {
    /// The dispatcher's SQLite database. Under the XDG state dir when
    /// unset (`$XDG_STATE_HOME/rhema/claw/dispatch.db`).
    pub db: Option<PathBuf>,
    /// The root the handler's per-thread workspaces live under. Under the
    /// XDG state dir when unset.
    pub workspace_root: Option<PathBuf>,
    /// The root of the resident tier's durable memory store. Under the
    /// XDG state dir when unset (`$XDG_STATE_HOME/rhema/claw/memory`).
    pub memory_root: Option<PathBuf>,
    /// The root of the bootstrap store — SOUL.md, USER.md,
    /// ENVIRONMENT.md. Under the XDG state dir when unset
    /// (`$XDG_STATE_HOME/rhema/claw/bootstrap`).
    pub bootstrap_root: Option<PathBuf>,
    /// Seconds between detect passes of the resident loop.
    pub poll_interval_secs: u64,
    /// How many tool rounds one chat turn may run before it is cut off
    /// with a failed outcome, the conversation intact. A follow-up
    /// message keeps going. [`DEFAULT_MAX_TURN_STEPS`] when unset.
    pub max_turn_steps: u32,
    /// The handler's model backend: `google`, `llama-cpp`, `openrouter`,
    /// `anthropic`, `openai`, or `zai`; google when unset. Keys resolve
    /// through the stored rhema config's default profile, env vars
    /// winning.
    pub provider: Option<String>,
    /// The model identifier; a per-provider default when unset.
    pub model: Option<String>,
    /// The HTTP surface's `host:port`. [`DEFAULT_LISTEN`] when unset.
    pub listen: Option<String>,
    /// The bounded request view over the claw's sessions — past a byte
    /// budget, older messages summarize into lines the agent's `zoom`
    /// tool can open. On when unset, matching the coding agent's
    /// default; `transcript: false` sends the whole history every
    /// step.
    pub transcript: Option<bool>,
    /// Bearer token for the `/v1` routes. Unset means the surface is
    /// open on its interface; set it when the port is reachable from
    /// anywhere but loopback. `/healthz` stays open either way.
    pub token: Option<String>,
    /// MCP server names held back from the claw's agents — the
    /// off switch (the global `mcp.yaml` is the claw's config scope;
    /// there are no profiles here). The wiring step filters before
    /// anything spawns and the prompt carries only what survives.
    #[serde(default, skip_serializing_if = "std::collections::BTreeSet::is_empty")]
    pub disabled_mcp_servers: std::collections::BTreeSet<String>,
    /// The scoped agent token the client tools hold: it answers only
    /// the data routes the tool contract allowlists — watches,
    /// notifications, tasks — never consents, sessions, or ingest, so
    /// handing it to the model's shell is safe (rhema-tools spec §5).
    /// Unset disables the client tools; the daemon then provisions no
    /// tool environment.
    pub agent_token: Option<String>,
    /// A directory of client-tool binaries (the rhema-tools project's)
    /// layered onto the agent's shell PATH. Unset leaves PATH alone.
    pub tools_bin: Option<PathBuf>,
    /// The ingest credential: a third bearer class, handed to
    /// integrations (a sidecar poller, an OS cron job) so they can hand
    /// events to `/v1/ingest` — and do nothing else. Write-only by
    /// construction: the middleware keeps it to the event door, and it
    /// must differ from both other tokens (boot refuses otherwise).
    /// Unset closes the door to credentials; the human token can still
    /// ingest.
    pub ingest_token: Option<String>,
}

impl ClawConfig {
    /// Whether the claw's sessions run the bounded request view: on
    /// unless the config says `transcript: false`.
    pub fn transcript_enabled(&self) -> bool {
        self.transcript != Some(false)
    }
}

impl Default for ClawConfig {
    fn default() -> Self {
        Self {
            db: None,
            workspace_root: None,
            memory_root: None,
            bootstrap_root: None,
            poll_interval_secs: DEFAULT_POLL_INTERVAL_SECS,
            max_turn_steps: DEFAULT_MAX_TURN_STEPS,
            provider: None,
            model: None,
            listen: None,
            token: None,
            agent_token: None,
            tools_bin: None,
            ingest_token: None,
            transcript: None,
            disabled_mcp_servers: std::collections::BTreeSet::new(),
        }
    }
}

impl ClawConfig {
    /// The default config path: `<config-base>/rhema/claw.yaml`, the same
    /// base the stored rhema config lives under.
    pub fn default_path() -> Result<PathBuf, ConfigError> {
        Ok(rhema_config::Home::discover()?.config.join("claw.yaml"))
    }

    /// The state dir: `$XDG_STATE_HOME/rhema/claw`, else
    /// `$HOME/.local/state/rhema/claw`.
    pub fn state_dir() -> Result<PathBuf, ConfigError> {
        match std::env::var("XDG_STATE_HOME") {
            Ok(dir) if !dir.trim().is_empty() => Ok(PathBuf::from(dir).join("rhema").join("claw")),
            _ => match std::env::var("HOME") {
                Ok(home) if !home.trim().is_empty() => Ok(PathBuf::from(home)
                    .join(".local")
                    .join("state")
                    .join("rhema")
                    .join("claw")),
                _ => Err(ConfigError::NoStateHome),
            },
        }
    }

    /// The dispatcher's SQLite path, defaulted under the state dir.
    pub fn db_path(&self) -> Result<PathBuf, ConfigError> {
        self.db
            .clone()
            .map(Ok)
            .unwrap_or_else(|| Self::state_dir().map(|dir| dir.join("dispatch.db")))
    }

    /// The workspace root, defaulted under the state dir.
    pub fn workspace_root(&self) -> Result<PathBuf, ConfigError> {
        self.workspace_root
            .clone()
            .map(Ok)
            .unwrap_or_else(|| Self::state_dir().map(|dir| dir.join("workspaces")))
    }

    /// The memory root, defaulted under the state dir: the durable store
    /// the resident tier's memory tools read and write.
    pub fn memory_path(&self) -> Result<PathBuf, ConfigError> {
        self.memory_root
            .clone()
            .map(Ok)
            .unwrap_or_else(|| Self::state_dir().map(|dir| dir.join("memory")))
    }

    /// The bootstrap store's root, defaulted under the state dir: the
    /// SOUL.md/USER.md the resident prompt renders and the
    /// ENVIRONMENT.md the boot pass generates.
    pub fn bootstrap_dir(&self) -> Result<PathBuf, ConfigError> {
        self.bootstrap_root
            .clone()
            .map(Ok)
            .unwrap_or_else(|| Self::state_dir().map(|dir| dir.join("bootstrap")))
    }

    /// The HTTP surface's address: the config's, or
    /// [`DEFAULT_LISTEN`] when unset.
    pub fn listen_addr(&self) -> &str {
        self.listen.as_deref().unwrap_or(DEFAULT_LISTEN)
    }

    /// The surface's startup posture check: a listen address reachable
    /// beyond loopback with no bearer token is an open agent API —
    /// anyone who can reach the port could run tools on this machine.
    /// Refused before the daemon binds, naming both facts and the fix.
    pub fn validate_surface(&self) -> Result<(), ConfigError> {
        let guarded = self
            .token
            .as_deref()
            .map(str::trim)
            .is_some_and(|token| !token.is_empty());
        if !guarded && !is_loopback_listen(self.listen_addr()) {
            return Err(ConfigError::OpenSurface {
                listen: self.listen_addr().to_owned(),
            });
        }
        Ok(())
    }

    /// The model identifier the handler runs: the config's, or the
    /// provider's headless default (the same fallbacks the rhema
    /// frontend's one-shot uses). An unknown provider string reads as
    /// google here; the binary rejects it when it resolves the backend.
    pub fn resolved_model(&self) -> String {
        self.model.clone().unwrap_or_else(|| {
            match self.provider.as_deref().unwrap_or("google") {
                "llama-cpp" => "local",
                "anthropic" => "claude-sonnet-4-5",
                "openai" => "gpt-5",
                // The Coding Plan's flagship, as Z.ai's own client spells it.
                "zai" => "GLM-5.3",
                _ => "gemini-3.8-flash",
            }
            .to_owned()
        })
    }

    /// Loads the config from `path`; a missing file is an error, not a
    /// default — a daemon started against a typo'd path must say so.
    pub fn load_from(path: &impl AsRef<std::path::Path>) -> Result<Self, ConfigError> {
        let raw = std::fs::read_to_string(path).map_err(|err| {
            if err.kind() == std::io::ErrorKind::NotFound {
                ConfigError::Missing(path.as_ref().to_path_buf())
            } else {
                ConfigError::Io(err)
            }
        })?;
        Ok(serde_yaml::from_str(&raw)?)
    }

    /// The startup checks: the credential classes do not collide, and
    /// the surface is open only where it is safe. Loud, before anything
    /// listens. Watch and calendar validity is not checked here — those
    /// are table rows, validated at their doors.
    pub fn validate(&self) -> Result<(), ConfigError> {
        // The agent token's safety is that it is scoped; making it the
        // human token silently unscopes everything the model's shell
        // holds. Boot refuses the configuration instead.
        if let (Some(token), Some(agent_token)) = (&self.token, &self.agent_token)
            && !token.is_empty()
            && token == agent_token
        {
            return Err(ConfigError::SameAgentToken);
        }
        // The ingest credential's safety is that it is write-only and
        // one route deep; a value shared with a wider class silently
        // widens it, so boot refuses the collision with either token.
        if let Some(ingest_token) = self.ingest_token.as_deref().filter(|t| !t.is_empty())
            && (Some(ingest_token) == self.token.as_deref()
                || Some(ingest_token) == self.agent_token.as_deref())
        {
            return Err(ConfigError::IngestTokenCollision);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_ingest_token_must_differ_from_both_other_credentials() {
        let config = ClawConfig {
            token: Some("same".to_owned()),
            ingest_token: Some("same".to_owned()),
            ..ClawConfig::default()
        };
        assert!(
            matches!(config.validate(), Err(ConfigError::IngestTokenCollision)),
            "an ingest credential sharing the human token is refused"
        );
        let config = ClawConfig {
            token: Some("human".to_owned()),
            agent_token: Some("same".to_owned()),
            ..config
        };
        assert!(
            matches!(config.validate(), Err(ConfigError::IngestTokenCollision)),
            "an ingest credential sharing the agent token is refused"
        );
        let config = ClawConfig {
            agent_token: Some("agent".to_owned()),
            ingest_token: Some("ingest".to_owned()),
            ..config
        };
        assert!(config.validate().is_ok(), "three distinct tokens pass");
        // An empty ingest token is no token: no collision to refuse.
        let blank = ClawConfig {
            ingest_token: Some(String::new()),
            ..ClawConfig::default()
        };
        assert!(blank.validate().is_ok(), "a blank ingest token is unset");
    }

    #[test]
    fn the_agent_token_must_differ_from_the_human_token() {
        let config = ClawConfig {
            token: Some("same".to_owned()),
            agent_token: Some("same".to_owned()),
            ..ClawConfig::default()
        };
        assert!(
            matches!(config.validate(), Err(ConfigError::SameAgentToken)),
            "one token playing both roles is refused"
        );
        let config = ClawConfig {
            agent_token: Some("other".to_owned()),
            ..config
        };
        assert!(config.validate().is_ok(), "two tokens pass");
        // An agent token with no human token at all is the tools-only
        // surface: legal, and scoped by construction.
        let tools_only = ClawConfig {
            agent_token: Some("agent".to_owned()),
            ..ClawConfig::default()
        };
        assert!(tools_only.validate().is_ok());
    }

    #[test]
    fn the_transcript_is_on_unless_the_config_turns_it_off() {
        assert!(ClawConfig::default().transcript_enabled());
        let off = ClawConfig {
            transcript: Some(false),
            ..ClawConfig::default()
        };
        assert!(!off.transcript_enabled());
        let explicit_on = ClawConfig {
            transcript: Some(true),
            ..ClawConfig::default()
        };
        assert!(explicit_on.transcript_enabled());
    }

    use super::*;

    #[test]
    fn a_full_file_parses_field_for_field() {
        let raw = "\
db: /var/lib/rhema/claw.db
workspace_root: /var/lib/rhema/claw-work
memory_root: /var/lib/rhema/claw-memory
bootstrap_root: /var/lib/rhema/claw-bootstrap
poll_interval_secs: 30
max_turn_steps: 512
provider: anthropic
model: claude-sonnet-4-5
listen: 192.168.1.10:7800
token: sekrit
ingest_token: door-key
";
        let config: ClawConfig = serde_yaml::from_str(raw).expect("parses");
        assert_eq!(config.poll_interval_secs, 30);
        assert_eq!(config.max_turn_steps, 512);
        assert_eq!(config.provider.as_deref(), Some("anthropic"));
        assert_eq!(config.model.as_deref(), Some("claude-sonnet-4-5"));
        assert_eq!(config.listen.as_deref(), Some("192.168.1.10:7800"));
        assert_eq!(config.token.as_deref(), Some("sekrit"));
        assert_eq!(
            config.db.as_deref(),
            Some(std::path::Path::new("/var/lib/rhema/claw.db"))
        );
        assert_eq!(
            config.bootstrap_root.as_deref(),
            Some(std::path::Path::new("/var/lib/rhema/claw-bootstrap"))
        );
    }

    #[test]
    fn an_unknown_key_is_rejected_and_named() {
        let err = serde_yaml::from_str::<ClawConfig>("listne: 127.0.0.1:7799\n")
            .expect_err("a typo'd key must not read as its default");
        assert!(
            err.to_string().contains("listne") && err.to_string().contains("expected one of"),
            "the error names the key: {err}"
        );
        // A config still carrying the old watches seed is refused
        // outright: the table rules now, and a silent seed would look
        // like it worked.
        let err = serde_yaml::from_str::<ClawConfig>(
            "watches:\n  - forge: github\n    repo: acme/widgets\n",
        )
        .expect_err("the old watch seed is not config anymore");
        assert!(
            err.to_string().contains("watches"),
            "the error names the retired key: {err}"
        );
    }

    #[test]
    fn defaults_fill_what_the_file_omits() {
        let config: ClawConfig = serde_yaml::from_str("listen: 127.0.0.1:7799\n").expect("parses");
        assert_eq!(config.poll_interval_secs, DEFAULT_POLL_INTERVAL_SECS);
        assert_eq!(config.max_turn_steps, DEFAULT_MAX_TURN_STEPS);
        assert_eq!(config.provider, None);
        assert_eq!(config.model, None);
        assert_eq!(config.db, None);
        assert_eq!(config.workspace_root, None);
        assert_eq!(config.bootstrap_root, None);
        assert_eq!(config.listen_addr(), DEFAULT_LISTEN);
        assert_eq!(config.token, None);
    }
    #[test]
    fn a_non_loopback_surface_demands_a_token() {
        let config = |listen: Option<&str>, token: Option<&str>| ClawConfig {
            listen: listen.map(str::to_owned),
            token: token.map(str::to_owned),
            ..ClawConfig::default()
        };
        // The default listen is loopback: open is fine.
        config(None, None)
            .validate_surface()
            .expect("default is safe");
        config(Some("localhost:7799"), None)
            .validate_surface()
            .expect("the localhost literal is loopback");
        config(Some("127.0.0.1:7799"), None)
            .validate_surface()
            .expect("an explicit loopback ip is loopback");
        config(Some("[::1]:7799"), None)
            .validate_surface()
            .expect("a bracketed ::1 is loopback");
        // A token guards a wide bind.
        config(Some("0.0.0.0:7799"), Some("sekrit"))
            .validate_surface()
            .expect("a token guards the wide bind");
        // No token on a wide bind is refused, naming the fix.
        let err = config(Some("0.0.0.0:7799"), None)
            .validate_surface()
            .expect_err("an unguarded wide bind is refused");
        assert!(
            err.to_string().contains("0.0.0.0:7799") && err.to_string().contains("set a token"),
            "got: {err}"
        );
        // A blank token counts as unset, everywhere else too.
        assert!(
            config(Some("192.168.1.10:7800"), Some("   "))
                .validate_surface()
                .is_err()
        );
    }

    #[test]
    fn loopback_detection_covers_the_listen_spellings() {
        for listen in [
            "localhost:7799",
            "LOCALHOST:7799",
            "127.0.0.1:7799",
            "127.8.8.8:80",
            "::1",
            "[::1]:7799",
            "[::1]",
        ] {
            assert!(is_loopback_listen(listen), "{listen} is loopback");
        }
        for listen in [
            "0.0.0.0:7799",
            "192.168.1.10:7800",
            "[::]:7799",
            ":7799",
            "example.com:7799",
            "not an address",
        ] {
            assert!(!is_loopback_listen(listen), "{listen} is not loopback");
        }
    }

    #[test]
    fn a_missing_file_is_named_not_defaulted() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("does-not-exist.yaml");
        let err = ClawConfig::load_from(&path).expect_err("missing is loud");
        assert!(matches!(err, ConfigError::Missing(_)), "got: {err:?}");
    }

    #[test]
    fn the_model_falls_back_per_provider() {
        let config = |provider: Option<&str>, model: Option<&str>| ClawConfig {
            provider: provider.map(str::to_owned),
            model: model.map(str::to_owned),
            ..ClawConfig::default()
        };
        assert_eq!(config(None, None).resolved_model(), "gemini-3.8-flash");
        assert_eq!(config(Some("llama-cpp"), None).resolved_model(), "local");
        assert_eq!(
            config(Some("anthropic"), None).resolved_model(),
            "claude-sonnet-4-5"
        );
        assert_eq!(config(Some("openai"), None).resolved_model(), "gpt-5");
        // An explicit model always wins, whatever the provider.
        assert_eq!(
            config(Some("openrouter"), Some("x/y")).resolved_model(),
            "x/y"
        );
    }
}
