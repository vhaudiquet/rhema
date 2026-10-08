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

use rhema_forge_agent::RepoRef;
use serde::{Deserialize, Serialize};

/// How often the resident loop runs a detect pass, when
/// [`ClawConfig::poll_interval_secs`] is unset.
pub const DEFAULT_POLL_INTERVAL_SECS: u64 = 60;

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
    /// The watch list is empty: claw guards threads, and this config
    /// guards none.
    #[error("no watches configured: claw has nothing to guard")]
    NoWatches,
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

    /// The production base URL; `None` for the self-hosted-only forges.
    pub fn default_base(self) -> Option<&'static str> {
        match self {
            ForgeKind::GitHub => Some(rhema_forge_agent::github::DEFAULT_BASE_URL),
            ForgeKind::Gitea => None,
            ForgeKind::GitLab => Some(rhema_forge_agent::gitlab::DEFAULT_BASE_URL),
        }
    }
}

/// One watched repository.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
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

/// The daemon's config file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "snake_case")]
pub struct ClawConfig {
    /// The dispatcher's SQLite database. Under the XDG state dir when
    /// unset (`$XDG_STATE_HOME/rhema/claw/dispatch.db`).
    pub db: Option<PathBuf>,
    /// The root the handler's per-thread workspaces live under. Under the
    /// XDG state dir when unset.
    pub workspace_root: Option<PathBuf>,
    /// Seconds between detect passes of the resident loop.
    pub poll_interval_secs: u64,
    /// The handler's model backend: `google`, `llama-cpp`, `openrouter`,
    /// `anthropic`, or `openai`; google when unset. Keys resolve through
    /// the stored rhema config's default profile, env vars winning.
    pub provider: Option<String>,
    /// The model identifier; a per-provider default when unset.
    pub model: Option<String>,
    /// What to watch: at least one entry, each validated at startup.
    pub watches: Vec<Watch>,
}

impl Default for ClawConfig {
    fn default() -> Self {
        Self {
            db: None,
            workspace_root: None,
            poll_interval_secs: DEFAULT_POLL_INTERVAL_SECS,
            provider: None,
            model: None,
            watches: Vec::new(),
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

    /// The startup checks: at least one watch, and every watch resolvable
    /// (shape, host, token). Loud, before anything polls.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.watches.is_empty() {
            return Err(ConfigError::NoWatches);
        }
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
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_full_file_parses_field_for_field() {
        let raw = "\
db: /var/lib/rhema/claw.db
workspace_root: /var/lib/rhema/claw-work
poll_interval_secs: 30
provider: anthropic
model: claude-sonnet-4-5
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
        assert_eq!(config.provider.as_deref(), Some("anthropic"));
        assert_eq!(config.model.as_deref(), Some("claude-sonnet-4-5"));
        assert_eq!(
            config.db.as_deref(),
            Some(std::path::Path::new("/var/lib/rhema/claw.db"))
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
    fn defaults_fill_what_the_file_omits() {
        let config: ClawConfig =
            serde_yaml::from_str("watches:\n  - forge: github\n    repo: acme/widgets\n")
                .expect("parses");
        assert_eq!(config.poll_interval_secs, DEFAULT_POLL_INTERVAL_SECS);
        assert_eq!(config.provider, None);
        assert_eq!(config.model, None);
        assert_eq!(config.db, None);
        assert_eq!(config.workspace_root, None);
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

        let err = config(vec![]).validate().expect_err("empty is loud");
        assert!(matches!(err, ConfigError::NoWatches));

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
