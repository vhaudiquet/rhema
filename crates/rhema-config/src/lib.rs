//! Persistent rhema settings: the config file backs login (stored provider
//! keys) and the last-used model choice.
//!
//! The file lives at `$XDG_CONFIG_HOME/rhema/config.json`, falling back to
//! `$HOME/.config/rhema/config.json`, written with `0600` permissions since
//! it holds credentials. Environment variables (`GEMINI_API_KEY`,
//! `GOOGLE_API_KEY`) always win over a stored key — a stored key is the
//! fallback, not the override.

#![forbid(unsafe_code)]
#![deny(missing_docs)]

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Why a config operation failed.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// Neither `XDG_CONFIG_HOME` nor `HOME` is set, so the config directory
    /// cannot be located.
    #[error("cannot locate the config directory: neither XDG_CONFIG_HOME nor HOME is set")]
    NoHome,
    /// Reading or writing the file failed.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// The file is not valid JSON or not the expected shape.
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

/// The persisted settings.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "snake_case")]
pub struct Config {
    /// The Google API key entered at login, when one is stored.
    pub google_api_key: Option<String>,
    /// The last-used model, when one was picked.
    pub model: Option<String>,
}

impl Config {
    /// The config directory (`<config-base>/rhema`).
    pub fn dir() -> Result<PathBuf, ConfigError> {
        match std::env::var("XDG_CONFIG_HOME") {
            Ok(dir) if !dir.trim().is_empty() => Ok(PathBuf::from(dir).join("rhema")),
            _ => match std::env::var("HOME") {
                Ok(home) if !home.trim().is_empty() => {
                    Ok(PathBuf::from(home).join(".config").join("rhema"))
                }
                _ => Err(ConfigError::NoHome),
            },
        }
    }

    /// The config file path (`<dir>/config.json`).
    pub fn path() -> Result<PathBuf, ConfigError> {
        Ok(Self::dir()?.join("config.json"))
    }

    /// Loads the config; a missing file is the default config, not an error.
    pub fn load() -> Result<Config, ConfigError> {
        match Self::path() {
            Ok(path) => Self::load_from(&path),
            Err(ConfigError::NoHome) => Ok(Config::default()),
            Err(err) => Err(err),
        }
    }

    /// Persists the config, creating the directory; the file is written with
    /// `0600` permissions on Unix because it holds credentials.
    pub fn save(&self) -> Result<(), ConfigError> {
        let dir = Self::dir()?;
        std::fs::create_dir_all(&dir)?;
        Self::save_to(&dir.join("config.json"), self)
    }

    /// [`Config::load`] against an explicit file (missing file is the
    /// default config).
    pub fn load_from(path: &impl AsRef<Path>) -> Result<Config, ConfigError> {
        match std::fs::read_to_string(path) {
            Ok(raw) => Ok(serde_json::from_str(&raw)?),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
            Err(err) => Err(err.into()),
        }
    }

    /// [`Config::save`] to an explicit file, with the same `0600` handling.
    pub fn save_to(path: &impl AsRef<Path>, config: &Config) -> Result<(), ConfigError> {
        let json = serde_json::to_string_pretty(config)?;
        // Mode applies at create time so the credentials never touch disk
        // world-readable.
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode_0600_on_unix()
            .open(path)?;
        use std::io::Write;
        file.write_all(json.as_bytes())?;
        Ok(())
    }

    /// The Google API key to use: `GEMINI_API_KEY`, then `GOOGLE_API_KEY`,
    /// then the stored key. Whitespace-only values count as unset.
    pub fn google_api_key(&self) -> Option<String> {
        resolve_google_key(|name| std::env::var(name).ok(), self.google_api_key.clone())
    }
}

trait OpenOptionsExt0600 {
    fn mode_0600_on_unix(&mut self) -> &mut Self;
}

impl OpenOptionsExt0600 for std::fs::OpenOptions {
    #[cfg(unix)]
    fn mode_0600_on_unix(&mut self) -> &mut Self {
        use std::os::unix::fs::OpenOptionsExt;
        self.mode(0o600)
    }

    #[cfg(not(unix))]
    fn mode_0600_on_unix(&mut self) -> &mut Self {
        self
    }
}

/// Pure key resolution: `env` looks up one variable name; the first
/// non-blank of `GEMINI_API_KEY`, `GOOGLE_API_KEY`, `stored` wins.
fn resolve_google_key(
    env: impl Fn(&str) -> Option<String>,
    stored: Option<String>,
) -> Option<String> {
    ["GEMINI_API_KEY", "GOOGLE_API_KEY"]
        .into_iter()
        .filter_map(env)
        .map(|value| value.trim().to_owned())
        .find(|value| !value.is_empty())
        .or(stored
            .map(|value| value.trim().to_owned())
            .filter(|v| !v.is_empty()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_key_wins_over_stored_and_blank_counts_as_unset() {
        let env = |name: &str| match name {
            "GEMINI_API_KEY" => Some("  ".into()),
            "GOOGLE_API_KEY" => Some("from-env".into()),
            _ => None,
        };
        assert_eq!(
            resolve_google_key(env, Some("stored".into())),
            Some("from-env".into())
        );
        assert_eq!(
            resolve_google_key(|_| None, Some(" stored ".into())),
            Some("stored".into())
        );
        assert_eq!(resolve_google_key(|_| None, Some("  ".into())), None);
        assert_eq!(resolve_google_key(|_| None, None), None);
    }

    #[test]
    fn config_round_trips_through_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        let config = Config {
            google_api_key: Some("key-123".into()),
            model: Some("gemini-3.8-flash".into()),
        };
        Config::save_to(&path, &config).unwrap();
        let loaded = Config::load_from(&path).unwrap();
        assert_eq!(loaded, config);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "credentials file must be 0600");
        }
    }

    #[test]
    fn a_missing_file_loads_as_the_default_config() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("does-not-exist.json");
        assert_eq!(Config::load_from(&path).unwrap(), Config::default());
    }
}
