//! The frontend bundle: home, settings store, and secret store, the
//! three decisions collected at one construction site.

use crate::error::ConfigError;
use crate::home::Home;
use crate::secrets;
use crate::store::{Store, YamlStore};

/// The platform a frontend hands the framework: the directory
/// conventions, the settings store, and the secret store, the three
/// decisions that belong to the frontend, collected at one construction
/// site so nothing downstream resolves a path or a file on its own.
pub struct Platform {
    /// The directory conventions.
    pub home: Home,
    /// The settings store.
    pub store: std::sync::Arc<dyn Store>,
    /// The credential store: the encrypted file backend under this
    /// home's config and state roots.
    pub secrets: std::sync::Arc<dyn secrets::SecretStore>,
}

impl Platform {
    /// The reference frontend's platform: the discovered [`Home`],
    /// the YAML file, and the encrypted file store under that home.
    pub fn rhema() -> Result<Self, ConfigError> {
        let home = Home::discover()?;
        let secrets = home.secret_store();
        Ok(Self {
            home,
            store: std::sync::Arc::new(YamlStore::discover()?),
            secrets,
        })
    }
}
