//! The claw config file: what to watch, where state lives, and which
//! model answers the dispatches.
//!
//! YAML at `~/.config/rhema/claw.yaml` (`--config` overrides). Paths and
//! credentials fail loudly at startup: a watch with no token, a gitea
//! watch with no host, or an unresolvable state directory never starts a
//! daemon that would poll half-configured. Forge tokens fall back to the
//! per-forge environment variable; the model backend's keys resolve
//! through `rhema-config` exactly as the TUI's do (env first, stored
//! second) and are validated in the binary's startup, not here — the
//! daemon library takes the built provider, so tests can inject a mock.

use std::path::PathBuf;

use rhema_forge_integration::RepoRef;
use serde::{Deserialize, Serialize};

// The calendar feed types are the integration's: the reminder source
// polls them there, and the claw's config only describes them.
pub use rhema_calendar_integration::reminder::{
    CalendarKind, CalendarWatch, DEFAULT_HORIZON_HOURS, DEFAULT_LEAD_MINUTES,
};

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
    /// The config file does not exist. Claw has nothing to guess from: a
    /// daemon without its watch list would silently watch nothing.
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
    /// One watch is unusable; the daemon refuses to start rather than
    /// poll a half-configured watch set.
    #[error("watch #{index} ({forge} {repo}): {reason}")]
    Watch {
        /// The watch's position in the config file, zero-based.
        index: usize,
        /// The watch's forge kind.
        forge: String,
        /// The watch's `owner/name` as written.
        repo: String,
        /// What is missing or malformed.
        reason: String,
    },
    /// One calendar watch is unusable; the daemon refuses to start
    /// rather than poll a half-configured calendar.
    #[error("calendar #{index} ({name}): {reason}")]
    Calendar {
        /// The calendar's position in the config file, zero-based.
        index: usize,
        /// The calendar's name as written.
        name: String,
        /// What is missing or malformed.
        reason: String,
    },
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

/// Which forge a watch polls. The wire spelling is the source name the
/// dispatcher registers and the resource key carries (`github:acme/w#7`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ForgeKind {
    /// GitHub (api.github.com, or a GitHub Enterprise host).
    GitHub,
    /// Gitea — self-hosted; a watch must name its instance.
    Gitea,
    /// GitLab (gitlab.com, or a self-hosted instance).
    GitLab,
}

impl ForgeKind {
    /// Every kind the claw ships a source for, in registration order.
    /// A kind missing here has no watch poller, whatever the table
    /// holds.
    pub const ALL: [ForgeKind; 3] = [ForgeKind::GitHub, ForgeKind::Gitea, ForgeKind::GitLab];

    /// The source name this kind registers and dispatches under.
    pub fn name(self) -> &'static str {
        match self {
            ForgeKind::GitHub => "github",
            ForgeKind::Gitea => "gitea",
            ForgeKind::GitLab => "gitlab",
        }
    }

    /// The environment variable the token falls back to.
    pub fn token_env(self) -> &'static str {
        match self {
            ForgeKind::GitHub => "GITHUB_TOKEN",
            ForgeKind::Gitea => "GITEA_TOKEN",
            ForgeKind::GitLab => "GITLAB_TOKEN",
        }
    }

    /// Parses a forge name as the config, the HTTP surface, and the
    /// watch tools spell it: the source name (`github`, `gitea`,
    /// `gitlab`). `None` for anything else.
    pub fn parse(name: &str) -> Option<Self> {
        match name.trim() {
            "github" => Some(ForgeKind::GitHub),
            "gitea" => Some(ForgeKind::Gitea),
            "gitlab" => Some(ForgeKind::GitLab),
            _ => None,
        }
    }

    /// The production base URL; `None` for the self-hosted-only forges.
    pub fn default_base(self) -> Option<&'static str> {
        match self {
            ForgeKind::GitHub => Some(rhema_forge_integration::github::DEFAULT_BASE_URL),
            ForgeKind::Gitea => None,
            ForgeKind::GitLab => Some(rhema_forge_integration::gitlab::DEFAULT_BASE_URL),
        }
    }
}

/// One watched repository. Unknown keys are rejected: a typo'd key
/// must not silently read as unset.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct Watch {
    /// Which forge this watch polls.
    pub forge: ForgeKind,
    /// The repository, `owner/name` (GitLab: `namespace/project`).
    pub repo: String,
    /// The instance base URL. Required for gitea (self-hosted only);
    /// github and gitlab default to their production bases.
    pub host: Option<String>,
    /// The forge token. Falls back to `GITHUB_TOKEN` / `GITEA_TOKEN` /
    /// `GITLAB_TOKEN`; a blank either way counts as unset.
    pub token: Option<String>,
}

impl Watch {
    /// The repository coordinates, splitting `owner/name`. GitLab's owner
    /// keeps its full namespace path, which is why the split is lazy: the
    /// first `/` separates owner from name, never a deeper one.
    pub fn repo_ref(&self) -> Result<RepoRef, String> {
        let (owner, name) = self
            .repo
            .split_once('/')
            .ok_or_else(|| format!("repo must be owner/name, got {:?}", self.repo))?;
        if owner.is_empty() || name.is_empty() {
            return Err(format!("repo must be owner/name, got {:?}", self.repo));
        }
        Ok(RepoRef::new(owner, name))
    }

    /// The token to poll with: the config value, then the forge's env
    /// variable; whitespace-only counts as unset everywhere.
    pub fn resolve_token(&self) -> Option<String> {
        // A closure, not `std::env::var` itself: the generic fn does not
        // implement `Fn(&str)` for all lifetimes.
        resolve_token(
            |name| std::env::var(name),
            self.forge,
            self.token.as_deref(),
        )
    }
}

/// Pure token resolution: the config value first, then the forge's env
/// variable; the first non-blank wins. Parameterized over the env lookup
/// so tests can hold the environment still.
pub fn resolve_token(
    get_env: impl Fn(&str) -> Result<String, std::env::VarError>,
    forge: ForgeKind,
    stored: Option<&str>,
) -> Option<String> {
    stored
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .map(str::to_owned)
        .or_else(|| {
            get_env(forge.token_env())
                .ok()
                .map(|token| token.trim().to_owned())
                .filter(|token| !token.is_empty())
        })
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
    /// tool can open. On when unset; `transcript: false` sends the
    /// whole history every step, as the coding agent does by default.
    /// The claw turns it on because its chats and coding jobs never end
    /// by design: a stashed session only ever grows.
    pub transcript: Option<bool>,
    /// Bearer token for the `/v1` routes. Unset means the surface is
    /// open on its interface; set it when the port is reachable from
    /// anywhere but loopback. `/healthz` stays open either way.
    pub token: Option<String>,
    /// What to watch — the optional seed for the persistent watch table.
    /// Imported once, when the table is still empty; after that the
    /// table rules, and watches are managed in chat or over HTTP.
    pub watches: Vec<Watch>,
    /// The calendars to remind about. Config-file-only: a calendar is
    /// read-only ambient state, not an agent-managed resource, and a
    /// change takes the daemon restart it takes to edit the file.
    pub calendars: Vec<CalendarWatch>,
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
            transcript: None,
            watches: Vec::new(),
            calendars: Vec::new(),
        }
    }
}

impl ClawConfig {
    /// The default config path: `<config-base>/rhema/claw.yaml`, the same
    /// base the stored rhema config lives under.
    pub fn default_path() -> Result<PathBuf, ConfigError> {
        Ok(rhema_config::Config::dir()?.join("claw.yaml"))
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

    /// The startup checks: every seeded watch resolvable (shape, host,
    /// token). Loud, before anything polls. An empty list is fine now —
    /// the watch table is seeded from it, and the agent manages its own
    /// watches from there; a daemon that starts with nothing to guard
    /// still serves chat.
    pub fn validate(&self) -> Result<(), ConfigError> {
        for (index, watch) in self.watches.iter().enumerate() {
            let fail = |reason: String| ConfigError::Watch {
                index,
                forge: watch.forge.name().to_owned(),
                repo: watch.repo.clone(),
                reason,
            };
            watch.repo_ref().map_err(fail)?;
            if watch.forge == ForgeKind::Gitea
                && watch
                    .host
                    .as_deref()
                    .map(str::trim)
                    .filter(|h| !h.is_empty())
                    .is_none()
            {
                return Err(fail(
                    "gitea is self-hosted: set host to the instance base URL".to_owned(),
                ));
            }
            if watch.resolve_token().is_none() {
                return Err(fail(format!(
                    "no token: set token in the config or {}",
                    watch.forge.token_env()
                )));
            }
        }
        self.validate_calendars()?;
        Ok(())
    }

    /// The calendar half of the startup checks: every calendar named,
    /// complete for its kind, with leads a horizon can see.
    fn validate_calendars(&self) -> Result<(), ConfigError> {
        let mut seen = std::collections::HashSet::new();
        for (index, calendar) in self.calendars.iter().enumerate() {
            let fail = |reason: String| ConfigError::Calendar {
                index,
                name: calendar.name.clone(),
                reason,
            };
            if calendar.name.trim().is_empty() {
                return Err(fail("name must not be blank".to_owned()));
            }
            if !seen.insert(calendar.name.trim().to_owned()) {
                return Err(fail(
                    "duplicate name: reminder keys would collide".to_owned(),
                ));
            }
            match calendar.kind {
                CalendarKind::Ics => {
                    let url = calendar
                        .url
                        .as_deref()
                        .map(str::trim)
                        .filter(|url| !url.is_empty())
                        .ok_or_else(|| {
                            fail("ics is a feed URL: set url to the calendar's address".to_owned())
                        })?;
                    if !(url.starts_with("https://")
                        || url.starts_with("http://")
                        || url.starts_with("webcal://"))
                    {
                        return Err(fail(format!(
                            "url must be https://, http://, or webcal://, got {url:?}"
                        )));
                    }
                }
                CalendarKind::Caldav => {
                    let url = calendar
                        .url
                        .as_deref()
                        .map(str::trim)
                        .filter(|url| !url.is_empty())
                        .ok_or_else(|| {
                            fail(
                                "caldav is a collection: set url to the calendar's DAV \
                                 address"
                                    .to_owned(),
                            )
                        })?;
                    if !(url.starts_with("https://") || url.starts_with("http://")) {
                        return Err(fail(format!(
                            "url must be https:// or http://, got {url:?}"
                        )));
                    }
                    let has_user = calendar
                        .username
                        .as_deref()
                        .map(str::trim)
                        .is_some_and(|user| !user.is_empty());
                    if has_user && calendar.resolved_password().is_none() {
                        return Err(fail(
                            "no password: set password in the config or CALDAV_PASSWORD".to_owned(),
                        ));
                    }
                }
                CalendarKind::Google => {
                    if calendar
                        .calendar_id
                        .as_deref()
                        .map(str::trim)
                        .filter(|id| !id.is_empty())
                        .is_none()
                    {
                        return Err(fail(
                            "google needs a calendar: set calendar_id (\"primary\", an \
                             address, a hex id)"
                                .to_owned(),
                        ));
                    }
                    if calendar.resolved_api_key().is_none() && calendar.resolved_token().is_none()
                    {
                        return Err(fail(
                            "no credential: set api_key (public calendars) or token in the \
                             config, or CALENDAR_TOKEN"
                                .to_owned(),
                        ));
                    }
                }
            }
            // The horizon is what the poll can see; a lead at or past
            // it fires on an event that was never visible.
            let horizon_minutes = u64::from(calendar.horizon()) * 60;
            let longest = calendar.leads().into_iter().max().unwrap_or_default();
            if longest >= horizon_minutes {
                return Err(fail(format!(
                    "lead {longest}m is not under the {horizon_minutes}m horizon: a \
                     reminder cannot fire before its event is visible"
                )));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
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
watches:
  - forge: github
    repo: acme/widgets
  - forge: gitea
    repo: acme/barn
    host: https://git.example.net
    token: gitea-pat
  - forge: gitlab
    repo: group/subgroup/widgets
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
        assert_eq!(config.watches.len(), 3);
        assert_eq!(config.watches[0].forge, ForgeKind::GitHub);
        assert_eq!(config.watches[0].host, None);
        assert_eq!(config.watches[1].forge, ForgeKind::Gitea);
        assert_eq!(
            config.watches[1].host.as_deref(),
            Some("https://git.example.net")
        );
        assert_eq!(config.watches[1].token.as_deref(), Some("gitea-pat"));
        // GitLab's namespace path survives as the owner.
        assert_eq!(
            config.watches[2].repo_ref().expect("parses").path(),
            "group/subgroup/widgets"
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
        // The same strictness inside a watch.
        let err = serde_yaml::from_str::<ClawConfig>(
            "watches:\n  - forge: github\n    repo: acme/widgets\n    hst: https://git.example.net\n",
        )
        .expect_err("a typo'd watch key is loud too");
        assert!(err.to_string().contains("hst"), "got: {err}");
    }

    #[test]
    fn defaults_fill_what_the_file_omits() {
        let config: ClawConfig =
            serde_yaml::from_str("watches:\n  - forge: github\n    repo: acme/widgets\n")
                .expect("parses");
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
    fn a_calendar_section_parses_field_for_field() {
        let raw = "calendars:
  - name: home
    kind: ics
    url: https://cloud.example.net/remote.php/dav/calendars/personal.ics
    lead_minutes: [60, 15]
    horizon_hours: 48
  - name: work
    kind: google
    calendar_id: primary
    api_key: pub-key
  - name: shared
    kind: google
    calendar_id: abc123@group.calendar.google.com
    token: oauth-token
  - name: dav
    kind: caldav
    url: https://cloud.example.net/remote.php/dav/calendars/me/personal/
    username: me
    password: app-pass
";
        let config: ClawConfig = serde_yaml::from_str(raw).expect("parses");
        assert_eq!(config.calendars.len(), 4);
        let home = &config.calendars[0];
        assert_eq!(home.name, "home");
        assert_eq!(home.kind, CalendarKind::Ics);
        assert_eq!(home.leads(), vec![60, 15]);
        assert_eq!(home.horizon(), 48);
        assert_eq!(
            config.calendars[1].resolved_api_key().as_deref(),
            Some("pub-key")
        );
        assert_eq!(
            config.calendars[2].resolved_token().as_deref(),
            Some("oauth-token")
        );
        assert_eq!(config.calendars[3].kind, CalendarKind::Caldav);
        assert_eq!(config.calendars[3].username.as_deref(), Some("me"));
        assert_eq!(
            config.calendars[3].resolved_password().as_deref(),
            Some("app-pass")
        );
        config.validate().expect("a complete list validates");
    }

    #[test]
    fn calendar_defaults_fill_what_the_file_omits() {
        let config: ClawConfig = serde_yaml::from_str(
            "calendars:\n  - name: home\n    kind: ics\n    url: https://c.example.net/a.ics\n",
        )
        .expect("parses");
        assert_eq!(config.calendars[0].leads(), vec![DEFAULT_LEAD_MINUTES]);
        assert_eq!(config.calendars[0].horizon(), DEFAULT_HORIZON_HOURS);
        // A blank or zero lead list reads as the default, not as
        // silence.
        let config: ClawConfig = serde_yaml::from_str(
            "calendars:\n  - name: home\n    kind: ics\n    url: https://c.example.net/a.ics\n    lead_minutes: []\n",
        )
        .expect("parses");
        assert_eq!(config.calendars[0].leads(), vec![DEFAULT_LEAD_MINUTES]);
    }

    #[test]
    fn a_calendar_typo_is_rejected_and_named() {
        let err = serde_yaml::from_str::<ClawConfig>(
            "calendars:\n  - name: home\n    kind: ics\n    uurl: https://c.example.net/a.ics\n",
        )
        .expect_err("a typo'd calendar key is loud");
        assert!(err.to_string().contains("uurl"), "got: {err}");
    }

    #[test]
    fn calendar_validation_fails_loudly_and_specifically() {
        let config = |calendars: Vec<CalendarWatch>| ClawConfig {
            calendars,
            ..ClawConfig::default()
        };
        let ics = |name: &str, url: Option<&str>| CalendarWatch {
            name: name.to_owned(),
            kind: CalendarKind::Ics,
            url: url.map(str::to_owned),
            calendar_id: None,
            token: None,
            username: None,
            password: None,
            api_key: None,
            lead_minutes: None,
            horizon_hours: None,
        };
        let caldav = |name: &str, url: Option<&str>| CalendarWatch {
            name: name.to_owned(),
            kind: CalendarKind::Caldav,
            url: url.map(str::to_owned),
            calendar_id: None,
            token: None,
            username: None,
            password: None,
            api_key: None,
            lead_minutes: None,
            horizon_hours: None,
        };
        let google = |name: &str, id: Option<&str>, api_key: Option<&str>| CalendarWatch {
            name: name.to_owned(),
            kind: CalendarKind::Google,
            url: None,
            calendar_id: id.map(str::to_owned),
            token: None,
            username: None,
            password: None,
            api_key: api_key.map(str::to_owned),
            lead_minutes: None,
            horizon_hours: None,
        };

        let err = config(vec![ics("", Some("https://c.example.net/a.ics"))])
            .validate()
            .expect_err("a blank name is loud");
        assert!(err.to_string().contains("blank"), "got: {err}");

        let err = config(vec![
            ics("home", Some("https://c.example.net/a.ics")),
            ics("home", Some("webcal://c.example.net/b.ics")),
        ])
        .validate()
        .expect_err("a duplicate name is loud");
        assert!(err.to_string().contains("duplicate"), "got: {err}");

        let err = config(vec![ics("home", None)])
            .validate()
            .expect_err("a url-less feed is loud");
        assert!(err.to_string().contains("feed URL"), "got: {err}");

        let err = config(vec![ics("home", Some("ftp://c.example.net/a.ics"))])
            .validate()
            .expect_err("a junk scheme is loud");
        assert!(err.to_string().contains("https://"), "got: {err}");

        let err = config(vec![caldav("home", None)])
            .validate()
            .expect_err("a url-less collection is loud");
        assert!(err.to_string().contains("DAV"), "got: {err}");

        let err = config(vec![caldav("home", Some("webcal://c.example.net/dav/"))])
            .validate()
            .expect_err("webcal is not a dav spelling");
        assert!(err.to_string().contains("https://"), "got: {err}");

        let err = config(vec![google("work", None, Some("k"))])
            .validate()
            .expect_err("an id-less google calendar is loud");
        assert!(err.to_string().contains("calendar_id"), "got: {err}");

        let err = config(vec![google("work", Some("primary"), None)])
            .validate()
            .expect_err("a credential-less google calendar is loud");
        assert!(err.to_string().contains("CALENDAR_TOKEN"), "got: {err}");

        let mut late = ics("home", Some("https://c.example.net/a.ics"));
        late.lead_minutes = Some(vec![60]);
        late.horizon_hours = Some(1);
        let err = config(vec![late])
            .validate()
            .expect_err("a lead past the horizon is loud");
        assert!(err.to_string().contains("horizon"), "got: {err}");

        // A username with a password validates, as does a collection
        // with none of it (an open server) or a token instead.
        let mut basic = caldav("home", Some("https://c.example.net/dav/home/"));
        basic.username = Some("me".to_owned());
        basic.password = Some("app-pass".to_owned());
        let mut bearer = caldav("shared", Some("http://10.0.0.5/dav/shared/"));
        bearer.token = Some("sekrit".to_owned());
        config(vec![
            ics("feed", Some("webcal://c.example.net/a.ics")),
            basic,
            bearer,
            google("work", Some("primary"), Some("k")),
        ])
        .validate()
        .expect("a complete list validates");
    }

    #[test]
    fn the_caldav_password_falls_back_to_the_env_and_blank_counts_as_unset() {
        let env = |name: &str| match name {
            "CALDAV_PASSWORD" => Ok("from-env".to_owned()),
            _ => Err(std::env::VarError::NotPresent),
        };
        assert_eq!(
            CalendarWatch::resolve_password_with(env, Some(" from-config ")),
            Some("from-config".to_owned()),
            "the config password wins"
        );
        assert_eq!(
            CalendarWatch::resolve_password_with(env, None),
            Some("from-env".to_owned()),
            "the env password serves when no config password"
        );
        assert_eq!(
            CalendarWatch::resolve_password_with(
                |_| Err(std::env::VarError::NotPresent),
                Some(" ")
            ),
            None,
            "a blank password counts as unset"
        );
    }

    #[test]
    fn the_calendar_token_falls_back_to_the_env_and_blank_counts_as_unset() {
        let env = |name: &str| match name {
            "CALENDAR_TOKEN" => Ok("from-env".to_owned()),
            _ => Err(std::env::VarError::NotPresent),
        };
        assert_eq!(
            CalendarWatch::resolve_token_with(env, Some(" from-config ")),
            Some("from-config".to_owned()),
            "the config token wins"
        );
        assert_eq!(
            CalendarWatch::resolve_token_with(env, None),
            Some("from-env".to_owned()),
            "the env token serves when no config token"
        );
        assert_eq!(
            CalendarWatch::resolve_token_with(|_| Err(std::env::VarError::NotPresent), Some("  ")),
            None,
            "a blank token counts as unset"
        );
    }

    #[test]
    fn forge_names_parse_as_the_sources_spell_them() {
        assert_eq!(ForgeKind::parse("github"), Some(ForgeKind::GitHub));
        assert_eq!(ForgeKind::parse(" gitea "), Some(ForgeKind::Gitea));
        assert_eq!(ForgeKind::parse("gitlab"), Some(ForgeKind::GitLab));
        assert_eq!(ForgeKind::parse("sourcehut"), None);
        assert_eq!(ForgeKind::parse(""), None);
        for kind in [ForgeKind::GitHub, ForgeKind::Gitea, ForgeKind::GitLab] {
            assert_eq!(ForgeKind::parse(kind.name()), Some(kind), "{}", kind.name());
        }
    }

    #[test]
    fn the_config_token_wins_over_env_and_blank_counts_as_unset() {
        let env = |name: &str| match name {
            "GITHUB_TOKEN" => Ok("from-env".to_owned()),
            "GITEA_TOKEN" => Ok("   ".to_owned()),
            _ => Err(std::env::VarError::NotPresent),
        };
        assert_eq!(
            resolve_token(env, ForgeKind::GitHub, Some(" from-config ")),
            Some("from-config".to_owned()),
            "the config token wins"
        );
        assert_eq!(
            resolve_token(env, ForgeKind::GitHub, None),
            Some("from-env".to_owned()),
            "the env token serves when no config token"
        );
        assert_eq!(
            resolve_token(env, ForgeKind::Gitea, Some("  ")),
            None,
            "a blank env token counts as unset"
        );
        assert_eq!(
            resolve_token(
                |_| Err(std::env::VarError::NotPresent),
                ForgeKind::GitLab,
                None
            ),
            None
        );
    }

    #[test]
    fn validation_fails_a_watch_loudly_and_specifically() {
        let watch = |forge, repo: &str, host: Option<&str>, token: Option<&str>| Watch {
            forge,
            repo: repo.to_owned(),
            host: host.map(str::to_owned),
            token: token.map(str::to_owned),
        };
        let config = |watches: Vec<Watch>| ClawConfig {
            watches,
            ..ClawConfig::default()
        };

        // An empty list is a valid starting point now: the table is
        // seeded from it, and the agent manages watches from chat.
        config(vec![]).validate().expect("an empty list validates");

        let err = config(vec![watch(ForgeKind::GitHub, "acme", None, Some("t"))])
            .validate()
            .expect_err("ownerless repo is loud");
        assert!(err.to_string().contains("owner/name"), "got: {err}");

        let err = config(vec![watch(ForgeKind::Gitea, "acme/barn", None, Some("t"))])
            .validate()
            .expect_err("hostless gitea is loud");
        assert!(err.to_string().contains("self-hosted"), "got: {err}");

        let err = config(vec![watch(ForgeKind::GitLab, "acme/barn", None, None)])
            .validate()
            .expect_err("tokenless watch is loud");
        assert!(err.to_string().contains("GITLAB_TOKEN"), "got: {err}");

        config(vec![watch(
            ForgeKind::GitHub,
            "acme/widgets",
            None,
            Some("t"),
        )])
        .validate()
        .expect("a complete github watch validates");
        config(vec![watch(
            ForgeKind::Gitea,
            "acme/barn",
            Some("https://git.example.net"),
            Some("t"),
        )])
        .validate()
        .expect("a complete gitea watch validates");
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
