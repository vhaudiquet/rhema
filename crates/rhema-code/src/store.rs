//! The config persistence seam: the app's state machine calls this trait,
//! never the filesystem, so login/model persistence is testable without
//! touching `$XDG_CONFIG_HOME`.

use rhema_config::{Config, ConfigError};

/// Reads and writes the persisted rhema config.
pub trait ConfigStore: Send + Sync {
    /// Loads the config (a missing file is the default config, not an
    /// error, matching [`Config::load`]).
    fn load(&self) -> Result<Config, ConfigError>;

    /// Persists the config, creating the directory.
    fn save(&self, config: &Config) -> Result<(), ConfigError>;
}

/// The real store: the rhema config file on disk.
#[derive(Debug, Clone, Copy, Default)]
pub struct DiskStore;

impl ConfigStore for DiskStore {
    fn load(&self) -> Result<Config, ConfigError> {
        Config::load()
    }

    fn save(&self, config: &Config) -> Result<(), ConfigError> {
        config.save()
    }
}

/// An in-memory store for tests.
#[derive(Debug, Default)]
pub struct MemoryStore {
    config: std::sync::Mutex<Config>,
}

impl MemoryStore {
    /// A store holding `config`.
    pub fn new(config: Config) -> Self {
        Self {
            config: std::sync::Mutex::new(config),
        }
    }
}

impl ConfigStore for MemoryStore {
    fn load(&self) -> Result<Config, ConfigError> {
        Ok(self
            .config
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone())
    }

    fn save(&self, config: &Config) -> Result<(), ConfigError> {
        *self
            .config
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = config.clone();
        Ok(())
    }
}
