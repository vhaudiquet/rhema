//! A second frontend, end to end: `GodCode`, a terminal UI of its own,
//! built on the rhema framework. It owns its settings the way a
//! frontend must,
//!
//! - its own directory (`~/.godcode`), not the rhema one;
//! - its own format (JSON, here, anything serde speaks would do);
//! - its own encrypted secret store (`~/.godcode/secrets.age`), so its
//!   credentials never mix with the rhema frontend's;
//! - the framework's [`Settings`] as one embedded section, so profiles
//!   and the persisted switches survive the trip through a format the
//!   framework never sees.
//!
//! The framework's only requirements: embed the section, implement
//! [`Store`], hand the driver a [`Home`]. Run with `cargo run -p
//! rhema-config --example godcode`.

use rhema_config::{ConfigError, Home, Settings, Store};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// GodCode's whole settings document: its own keys beside the
/// framework's section, in its own format.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct GodCodeConfig {
    /// The framework section, verbatim, one field, one round trip.
    #[serde(default)]
    rhema: Settings,
    /// GodCode's own stuff; the framework never hears of it.
    accent: Option<String>,
    /// Whether GodCode's splash screen ever ran.
    #[serde(default)]
    onboarded: bool,
}

/// The store GodCode implements over its own file: JSON at
/// `~/.godcode/config.json`.
struct GodCodeStore {
    path: PathBuf,
}

impl Store for GodCodeStore {
    fn load(&self) -> Result<Settings, ConfigError> {
        let raw = match std::fs::read_to_string(&self.path) {
            Ok(raw) => raw,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Settings::default());
            }
            Err(err) => return Err(err.into()),
        };
        let doc: GodCodeConfig =
            serde_json::from_str(&raw).map_err(|err| ConfigError::Store(err.to_string()))?;
        Ok(doc.rhema)
    }

    fn save(&self, settings: &Settings) -> Result<(), ConfigError> {
        // Read the whole document back, replace the section, keep the
        // rest: GodCode's own keys survive a framework-section write.
        let mut doc = std::fs::read_to_string(&self.path)
            .ok()
            .and_then(|raw| serde_json::from_str::<GodCodeConfig>(&raw).ok())
            .unwrap_or_default();
        doc.rhema = settings.clone();
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).map_err(ConfigError::from)?;
        }
        let json = serde_json::to_string_pretty(&doc)
            .map_err(|err| ConfigError::Store(err.to_string()))?;
        std::fs::write(&self.path, json).map_err(ConfigError::from)
    }
}

fn main() -> Result<(), ConfigError> {
    // The three frontend-owned decisions, one construction site.
    let home = Home::relocated(dirs_home_dot_godcode()?)?;
    let store = GodCodeStore {
        path: home.config.join("config.json"),
    };
    let secrets = home.secret_store();

    // The section loads from a format the framework never parses.
    let mut settings = store.load()?;
    settings.set_for_profile(Some("work"), |profile| {
        profile.model = Some("godcode-favorite".into());
    });
    store.save(&settings)?;
    let reloaded = store.load()?;
    assert_eq!(reloaded, settings);

    // The one-way startup migration: no refs in this demo's fresh
    // section, so it is a no-op, but a real frontend gets its old
    // `godcode` keyring entries copied here.
    let _ = settings.import_legacy_secrets(
        &rhema_config::secrets::KeyringSecretReader::for_home(&home),
        secrets.as_ref(),
    );

    // The store is GodCode's: the ciphertext sits under `~/.godcode`,
    // the generated key under its own state root, invisible to the rhema
    // frontend's store.
    let probe_reachable = secrets.reachable();
    println!(
        "godcode: settings at {}, secret store at {}, key at {}, cache at {} ({})",
        home.config_file().display(),
        home.secrets_file().display(),
        home.secrets_key().display(),
        home.cache.display(),
        if probe_reachable {
            "reachable"
        } else {
            "absent"
        }
    );
    // The driver takes the three decisions as one platform handle.
    let platform = rhema_config::Platform {
        home,
        store: std::sync::Arc::new(store),
        secrets,
    };
    let _ = platform; // the assembly point for the host agent
    Ok(())
}

/// `~/.godcode` under the user's home, no external directories crate,
/// this is the whole of it.
fn dirs_home_dot_godcode() -> Result<PathBuf, ConfigError> {
    let home = std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .ok_or(ConfigError::NoHome)?;
    Ok(PathBuf::from(home).join(".godcode"))
}
