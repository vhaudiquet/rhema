//! The directory conventions a session reads, from the settings file
//! to the cross-agent skills folder.

use crate::error::ConfigError;
use std::path::{Path, PathBuf};

/// The directory conventions a session reads: where the settings
/// file, the user-wide skills, the subagent definitions, and the base
/// system prompt live, plus the user home, because the `.agents`
/// skills-sharing convention lives there and nowhere else. The state
/// and cache roots ride along: runtime artifacts (a daemon's sqlite,
/// saved sessions) belong to the same family and the same bases.
///
/// The family rule: the products of the rhema family share the XDG
/// bases and nest below the family name, the reference frontend at
/// `<config-base>/rhema`, the claw below it (`claw.yaml`,
/// `state/rhema/claw`), the ACP adapter's session cache below that.
/// A frontend from outside the family relocates the whole set
/// ([`Home::relocated`]): `~/.godcode` for its config, and its own
/// name under the state and cache bases, nothing of godcode's lands
/// under `rhema/`. The cross-agent convention stays machine-global
/// either way: `~/.agents/skills` is every tool's, not any frontend's.
/// Nothing downstream reaches for the environment again; it asks the
/// [`Home`] it was handed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Home {
    /// The user's home directory: where the `.agents/skills` sharing
    /// convention lives, every tool's skills, not any frontend's.
    pub home: PathBuf,
    /// The config base: `<config-base>/rhema` for the reference
    /// layout, the frontend's own directory for a relocated one.
    pub config: PathBuf,
    /// The state root: `$XDG_STATE_HOME/rhema`, else
    /// `~/.local/state/rhema`, runtime artifacts the user expects to
    /// survive a restart, the family's own directory of them.
    pub state: PathBuf,
    /// The cache root: `$XDG_CACHE_HOME/rhema`, else `~/.cache/rhema`
    ///, artifacts that may be lost without grief, saved sessions
    /// among them.
    pub cache: PathBuf,
}

impl Home {
    /// The reference layout: `config` under `XDG_CONFIG_HOME` (or
    /// `~/.config`), `state` under `XDG_STATE_HOME` (or
    /// `~/.local/state`), `cache` under `XDG_CACHE_HOME` (or
    /// `~/.cache`), each under the family name; `home` under `HOME`.
    /// The shape a machine running the rhema frontend presents.
    pub fn discover() -> Result<Self, ConfigError> {
        let state_base = match std::env::var("XDG_STATE_HOME") {
            Ok(dir) if !dir.trim().is_empty() => PathBuf::from(dir),
            _ => user_home()?.join(".local").join("state"),
        };
        let cache_base = match std::env::var("XDG_CACHE_HOME") {
            Ok(dir) if !dir.trim().is_empty() => PathBuf::from(dir),
            _ => user_home()?.join(".cache"),
        };
        Ok(Self {
            home: user_home()?,
            config: reference_config_base()?,
            state: state_base.join("rhema"),
            cache: cache_base.join("rhema"),
        })
    }

    /// A relocated config base under the discovered home: the seam
    /// for a frontend from outside the family. Skills, subagents, and
    /// the base prompt follow the config base; its state and cache
    /// take the frontend's own name under their bases, the config
    /// base's final component, leading dot stripped (`~/.godcode`
    /// keeps godcode out of `rhema/`), and the `.agents` convention
    /// stays machine-global.
    pub fn relocated(config: PathBuf) -> Result<Self, ConfigError> {
        let name = frontend_name(&config);
        let state_base = match std::env::var("XDG_STATE_HOME") {
            Ok(dir) if !dir.trim().is_empty() => PathBuf::from(dir),
            _ => user_home()?.join(".local").join("state"),
        };
        let cache_base = match std::env::var("XDG_CACHE_HOME") {
            Ok(dir) if !dir.trim().is_empty() => PathBuf::from(dir),
            _ => user_home()?.join(".cache"),
        };
        Ok(Self {
            home: user_home()?,
            state: state_base.join(&name),
            cache: cache_base.join(name),
            config,
        })
    }

    /// All roots explicit, containers and tests, where no
    /// environment variable may answer. State and cache take the
    /// config base's name under the home's default XDG shapes, the
    /// same derivation [`Self::relocated`] applies to bases it
    /// resolves itself.
    pub fn at(home: PathBuf, config: PathBuf) -> Self {
        let name = frontend_name(&config);
        Self {
            state: home.join(".local").join("state").join(&name),
            cache: home.join(".cache").join(name),
            home,
            config,
        }
    }

    /// The reference layout under an explicit home, the seam for a
    /// test isolating itself from the machine: `config` under
    /// `<home>/.config/rhema`, state under `<home>/.local/state/rhema`,
    /// cache under `<home>/.cache/rhema`, the shapes
    /// [`Self::discover`] resolves, no environment variable consulted.
    pub fn under_home(home: &Path) -> Self {
        Self {
            state: home.join(".local").join("state").join("rhema"),
            cache: home.join(".cache").join("rhema"),
            home: home.to_path_buf(),
            config: home.join(".config").join("rhema"),
        }
    }

    /// The settings file (`<config>/config.yaml`) under the reference
    /// YAML store; another format keeps its own name beside it.
    pub fn config_file(&self) -> PathBuf {
        self.config.join("config.yaml")
    }

    /// The encrypted secret store: the age ciphertext, under the
    /// config base. A config directory copied or synced without the
    /// state root carries no readable credential.
    pub fn secrets_file(&self) -> PathBuf {
        self.config.join("secrets.age")
    }

    /// The generated key that opens [`Self::secrets_file`], under the
    /// state root, deliberately apart from the ciphertext.
    pub fn secrets_key(&self) -> PathBuf {
        self.state.join("secrets.key")
    }

    /// The secret store for this home: the encrypted file backend,
    /// built over the two paths above.
    pub fn secret_store(&self) -> std::sync::Arc<dyn crate::secrets::SecretStore> {
        std::sync::Arc::new(crate::secrets::EncryptedFileSecretStore::for_home(self))
    }

    /// The OS keyring service name the pre-file-store versions wrote
    /// under: `rhema` for the reference frontend, the frontend's own
    /// name for a relocated one. The read-only importer reads this
    /// namespace; nothing writes there any more. A frontend that used a
    /// service of its own passes it to
    /// [`crate::secrets::KeyringSecretReader::named`] instead.
    pub fn legacy_keyring_service(&self) -> String {
        frontend_name(&self.config)
    }

    /// The user-wide skills directory (`<config>/skills`); a workspace
    /// can add more under its own root.
    pub fn skills(&self) -> PathBuf {
        self.config.join("skills")
    }

    /// The cross-agent skills directory (`<home>/.agents/skills`),
    /// where the skill-sharing convention keeps user-wide skills.
    /// Discovery reads it at the bottom of the shadowing order, so a
    /// rhema skill of the same name always wins.
    pub fn agents_skills(&self) -> PathBuf {
        self.home.join(".agents").join("skills")
    }

    /// The user-wide subagent definitions directory
    /// (`<config>/subagents`); a workspace can add more under its own
    /// `.rhema/subagents`.
    pub fn subagents(&self) -> PathBuf {
        self.config.join("subagents")
    }

    /// The editable base system prompt (`<config>/SYSTEM.md`). A
    /// `system edit` command seeds it with the built-in default,
    /// a `system restore` restores it; nothing is ever written at
    /// runtime, the file is an opt-in override.
    pub fn system_prompt(&self) -> PathBuf {
        self.config.join("SYSTEM.md")
    }
}

/// The user's home directory: `$HOME`, falling back to `$USERPROFILE`
///, windows does not set `HOME` (git-bash and MSYS do, cmd and
/// powershell never), so the windows convention answers second. Public:
/// every convention outside this crate that hangs off the home (the
/// sessions' cache dir, for one) resolves it the same one way.
pub fn user_home() -> Result<PathBuf, ConfigError> {
    match std::env::var("HOME").or_else(|_| std::env::var("USERPROFILE")) {
        Ok(home) if !home.trim().is_empty() => Ok(PathBuf::from(home)),
        _ => Err(ConfigError::NoHome),
    }
}

/// The frontend's own name, derived from its config base's final
/// component with any leading dot stripped (`~/.godcode` names
/// `godcode`); a base with no usable component keeps the family name.
fn frontend_name(config_base: &Path) -> String {
    config_base
        .file_name()
        .and_then(|name| name.to_str())
        .map(|name| name.strip_prefix('.').unwrap_or(name).to_owned())
        .unwrap_or_else(|| "rhema".to_owned())
}

/// The reference config base (`$XDG_CONFIG_HOME/rhema`, or
/// `~/.config/rhema`).
fn reference_config_base() -> Result<PathBuf, ConfigError> {
    match std::env::var("XDG_CONFIG_HOME") {
        Ok(dir) if !dir.trim().is_empty() => Ok(PathBuf::from(dir).join("rhema")),
        _ => Ok(user_home()?.join(".config").join("rhema")),
    }
}
#[cfg(test)]
mod tests;
