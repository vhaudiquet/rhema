//! The frontend bundle: home, store, and keychain namespace, the
//! three decisions collected at one construction site.

use crate::error::ConfigError;
use crate::home::Home;
use crate::secrets;
use crate::store::{Store, YamlStore};

/// The platform a frontend hands the framework: the directory
/// conventions, the settings store, and the keychain namespace, the
/// three decisions that belong to the frontend, collected at one
/// construction site so nothing downstream resolves a path, a file,
/// or a service name on its own.
pub struct Platform {
    /// The directory conventions.
    pub home: Home,
    /// The settings store.
    pub store: std::sync::Arc<dyn Store>,
    /// The credential store: entries live under the frontend's own
    /// service name, so two frontends never share credentials by
    /// accident.
    pub secrets: std::sync::Arc<dyn secrets::SecretStore>,
}

impl Platform {
    /// The reference frontend's platform: the discovered [`Home`],
    /// the YAML file, and the `rhema` keychain namespace.
    pub fn rhema() -> Result<Self, ConfigError> {
        Ok(Self {
            home: Home::discover()?,
            store: std::sync::Arc::new(YamlStore::discover()?),
            secrets: std::sync::Arc::new(secrets::KeyringSecretStore::named(
                secrets::RHEMA_SERVICE,
            )),
        })
    }
}
