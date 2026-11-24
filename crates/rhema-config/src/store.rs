//! The settings-storage seam and its reference YAML backend.

use crate::error::ConfigError;
use crate::home::Home;
use crate::privacy::{private_dir, private_write_atomic};
use crate::settings::Settings;
use std::path::PathBuf;

/// The settings-storage seam: where the [`Settings`] section comes
/// from and goes. The framework ships the YAML file store; a frontend
/// that owns its own document, its own directory, its own format,
/// implements this over its embed of the section, and nothing
/// downstream knows a file or a format exists.
pub trait Store: Send + Sync {
    /// The stored settings; a missing store is the default settings,
    /// not an error.
    fn load(&self) -> Result<Settings, ConfigError>;
    /// Persists the settings. A frontend co-locating the section with
    /// its own keys re-serializes its whole document around it, so the
    /// document's other sections survive, best effort, as file
    /// semantics allow.
    fn save(&self, settings: &Settings) -> Result<(), ConfigError>;
}

/// The reference [`Store`]: `config.yaml` in YAML, the shape the file
/// has always had. Atomic and `0600`, the section can hold literal
/// credentials, with the directory created `0700`; both modes are
/// re-asserted on every save, so a file that predates the rule, or
/// was widened elsewhere, tightens again.
#[derive(Debug, Clone)]
pub struct YamlStore {
    /// The file the section round-trips through.
    pub path: PathBuf,
}

impl YamlStore {
    /// A store over an explicit file.
    pub fn at(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// The reference file under the discovered [`Home`].
    pub fn discover() -> Result<Self, ConfigError> {
        Ok(Self::at(Home::discover()?.config_file()))
    }
}

impl Store for YamlStore {
    fn load(&self) -> Result<Settings, ConfigError> {
        let raw = match std::fs::read_to_string(&self.path) {
            Ok(raw) => raw,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Settings::default());
            }
            Err(err) => return Err(err.into()),
        };
        Ok(serde_yaml::from_str(&raw)?)
    }

    fn save(&self, settings: &Settings) -> Result<(), ConfigError> {
        if let Some(parent) = self.path.parent() {
            private_dir(parent)?;
        }
        let yaml = serde_yaml::to_string(settings)?;
        private_write_atomic(&self.path, yaml.as_bytes())?;
        Ok(())
    }
}
#[cfg(test)]
mod tests;
