//! Persistent rhema settings: named profiles for contexts like work and
//! personal, plus a fallback section that legacy config files already
//! deserialize into.
//!
//! The file lives at `$XDG_CONFIG_HOME/rhema/config.yaml`, falling back to
//! `$HOME/.config/rhema/config.json`, written with `0600` permissions since
//! it holds credentials. A legacy `config.json` is read when no YAML file
//! exists but never written. Environment variables (`GEMINI_API_KEY`,
//! `GOOGLE_API_KEY`) always win over a stored key — a stored key is the
//! fallback, not the override.

#![forbid(unsafe_code)]
#![deny(missing_docs)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

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
    /// The YAML file is not valid or not the expected shape.
    #[error(transparent)]
    Yaml(#[from] serde_yaml::Error),
}

/// One named set of provider settings. A profile overrides the top-level
/// section field-wise, so a profile only needs the fields where it
/// differs (typically the key and the model).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "snake_case")]
pub struct Profile {
    /// Which provider serves turns: `google`, `llama-cpp`, or `openrouter`.
    pub provider: Option<String>,
    /// The Google API key entered at login, when one is stored.
    pub google_api_key: Option<String>,
    /// The last-used model, when one was picked.
    pub model: Option<String>,
    /// The llama.cpp server base URL (e.g. `http://localhost:8080`).
    pub llama_cpp_url: Option<String>,
    /// Optional bearer key for the llama.cpp server, when it asks for one.
    pub llama_cpp_api_key: Option<String>,
    /// The OpenRouter API key entered at login, when one is stored.
    pub openrouter_api_key: Option<String>,
    /// The color this profile's name wears in the UI, as `#RRGGBB`.
    /// Unset means the UI picks a tone from the theme by profile name.
    pub color: Option<String>,
}

impl Profile {
    /// The Google API key to use: `GEMINI_API_KEY`, then `GOOGLE_API_KEY`,
    /// then the stored key. Whitespace-only values count as unset.
    pub fn google_api_key(&self) -> Option<String> {
        resolve_key(
            |name| std::env::var(name).ok(),
            &["GEMINI_API_KEY", "GOOGLE_API_KEY"],
            self.google_api_key.clone(),
        )
    }

    /// The llama.cpp server URL to use: `LLAMA_CPP_URL`, then the stored
    /// URL, then `http://localhost:8080`.
    pub fn llama_cpp_url(&self) -> String {
        resolve_key(
            |name| std::env::var(name).ok(),
            &["LLAMA_CPP_URL"],
            self.llama_cpp_url.clone(),
        )
        .unwrap_or_else(|| "http://localhost:8080".to_owned())
    }

    /// The llama.cpp bearer key to use: `LLAMA_CPP_API_KEY`, then the
    /// stored key; `None` when the server is open.
    pub fn llama_cpp_api_key(&self) -> Option<String> {
        resolve_key(
            |name| std::env::var(name).ok(),
            &["LLAMA_CPP_API_KEY"],
            self.llama_cpp_api_key.clone(),
        )
    }

    /// The OpenRouter key to use: `OPENROUTER_API_KEY`, then the stored
    /// key.
    pub fn openrouter_api_key(&self) -> Option<String> {
        resolve_key(
            |name| std::env::var(name).ok(),
            &["OPENROUTER_API_KEY"],
            self.openrouter_api_key.clone(),
        )
    }

    /// Whether this profile can build a backend: it names a provider and
    /// carries the credential that provider needs (a llama.cpp server
    /// works keyless; google and openrouter need a key, env included).
    pub fn has_credentials(&self) -> bool {
        match self.provider.as_deref() {
            Some("llama-cpp") => true,
            Some("openrouter") => self.openrouter_api_key().is_some(),
            _ => self.google_api_key().is_some(),
        }
    }
}

/// The persisted settings: the active profile name plus named profiles.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "snake_case")]
pub struct Config {
    /// The profile used when `--profile` does not name one.
    #[serde(
        default = "default_active_profile",
        skip_serializing_if = "Option::is_none"
    )]
    pub profile: Option<String>,
    /// Named profiles; each overrides the top-level section field-wise.
    #[serde(
        default = "default_profiles",
        skip_serializing_if = "BTreeMap::is_empty"
    )]
    pub profiles: BTreeMap<String, Profile>,
    /// The top-level fallback section — also what legacy config files
    /// deserialize into.
    #[serde(flatten)]
    pub default: Profile,
}

/// The active profile on fresh setups and files that omit the key.
fn default_active_profile() -> Option<String> {
    Some("personal".to_owned())
}

/// The profiles on fresh setups and files that omit the key.
fn default_profiles() -> BTreeMap<String, Profile> {
    BTreeMap::from([
        ("personal".to_owned(), Profile::default()),
        ("work".to_owned(), Profile::default()),
    ])
}

impl Default for Config {
    /// Fresh setups ship with the two everyday profiles and `personal`
    /// active. Loaded files missing the keys get the same treatment via
    /// the field-level serde defaults.
    fn default() -> Self {
        Self {
            profile: default_active_profile(),
            profiles: default_profiles(),
            default: Profile::default(),
        }
    }
}

impl Config {
    /// The settings that apply for `flag` (or the stored active profile).
    ///
    /// ISOLATION: a profile that exists in the map is authoritative — its
    /// section alone applies, with no field-wise fallback to the top-level
    /// section. Work must never inherit personal's model or keys. The
    /// top-level section applies only when the named profile has no entry
    /// (configs from before profiles existed).
    pub fn effective(&self, flag: Option<&str>) -> Profile {
        let chosen = flag.or(self.profile.as_deref());
        match chosen.and_then(|name| self.profiles.get(name)) {
            Some(profile) => profile.clone(),
            None => self.default.clone(),
        }
    }

    /// Mutates the section that writes land in for `flag` (or the stored
    /// active profile): the named profile when it exists or is new, else
    /// the top-level section.
    pub fn set_for_profile(&mut self, flag: Option<&str>, apply: impl FnOnce(&mut Profile)) {
        match flag.or(self.profile.as_deref()) {
            Some(name) => apply(self.profiles.entry(name.to_owned()).or_default()),
            None => apply(&mut self.default),
        }
    }

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

    /// The config file path (`<dir>/config.yaml`).
    pub fn path() -> Result<PathBuf, ConfigError> {
        Ok(Self::dir()?.join("config.yaml"))
    }

    /// Loads the config; a missing file is the default config, not an
    /// error. Falls back to the legacy JSON file when no YAML exists.
    pub fn load() -> Result<Config, ConfigError> {
        match Self::path() {
            Ok(path) => Self::load_from(&path),
            Err(ConfigError::NoHome) => Ok(Config::default()),
            Err(err) => Err(err),
        }
    }

    /// Persists the config, creating the directory; the file is written
    /// with `0600` permissions on Unix because it holds credentials.
    pub fn save(&self) -> Result<(), ConfigError> {
        let dir = Self::dir()?;
        std::fs::create_dir_all(&dir)?;
        Self::save_to(&dir.join("config.yaml"), self)
    }

    /// [`Config::load`] against an explicit file (missing file is the
    /// default config).
    pub fn load_from(path: &impl AsRef<Path>) -> Result<Config, ConfigError> {
        let raw = match std::fs::read_to_string(path) {
            Ok(raw) => raw,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Config::default());
            }
            Err(err) => return Err(err.into()),
        };
        Ok(serde_yaml::from_str(&raw)?)
    }

    /// [`Config::save`] to an explicit file, with the same `0600` handling.
    pub fn save_to(path: &impl AsRef<Path>, config: &Config) -> Result<(), ConfigError> {
        let yaml = serde_yaml::to_string(config)?;
        // Mode applies at create time so the credentials never touch disk
        // world-readable.
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode_0600_on_unix()
            .open(path)?;
        use std::io::Write;
        file.write_all(yaml.as_bytes())?;
        Ok(())
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

/// Pure key resolution: the first non-blank of the named environment
/// variables, then `stored`; whitespace-only values count as unset.
fn resolve_key(
    env: impl Fn(&str) -> Option<String>,
    names: &[&str],
    stored: Option<String>,
) -> Option<String> {
    names
        .iter()
        .filter_map(|name| env(name))
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
            resolve_key(
                env,
                &["GEMINI_API_KEY", "GOOGLE_API_KEY"],
                Some("stored".into())
            ),
            Some("from-env".into())
        );
        assert_eq!(
            resolve_key(|_| None, &["GEMINI_API_KEY"], Some(" stored ".into())),
            Some("stored".into())
        );
        assert_eq!(
            resolve_key(|_| None, &["GEMINI_API_KEY"], Some("  ".into())),
            None
        );
        assert_eq!(resolve_key(|_| None, &["GEMINI_API_KEY"], None), None);
    }

    #[test]
    fn config_round_trips_through_disk_as_yaml() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.yaml");
        let config = Config {
            default: Profile {
                provider: Some("llama-cpp".into()),
                google_api_key: Some("key-123".into()),
                model: Some("gemini-3.8-flash".into()),
                llama_cpp_url: Some("http://localhost:8080".into()),
                llama_cpp_api_key: None,
                openrouter_api_key: None,
                color: None,
            },
            ..Config::default()
        };
        Config::save_to(&path, &config).unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.contains("google_api_key:"), "yaml on disk: {raw}");
        assert!(!raw.contains('{'), "not json: {raw}");
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
        let path = dir.path().join("does-not-exist.yaml");
        assert_eq!(Config::load_from(&path).unwrap(), Config::default());
    }

    #[test]
    fn fresh_setups_ship_personal_and_work_profiles() {
        let config = Config::default();
        assert_eq!(config.profile.as_deref(), Some("personal"));
        assert!(config.profiles.contains_key("personal"));
        assert!(config.profiles.contains_key("work"));
        // A loaded file missing the keys gets the same seeding.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.yaml");
        std::fs::write(&path, "model: m\n").unwrap();
        let loaded = Config::load_from(&path).unwrap();
        assert_eq!(loaded.profile.as_deref(), Some("personal"));
        assert!(loaded.profiles.contains_key("work"));
        assert_eq!(loaded.default.model.as_deref(), Some("m"));
    }

    #[test]
    fn profiles_are_isolated_from_the_default_section_and_each_other() {
        let config = Config {
            profile: Some("work".into()),
            default: Profile {
                provider: Some("google".into()),
                google_api_key: Some("personal-key".into()),
                model: Some("personal-model".into()),
                ..Profile::default()
            },
            profiles: BTreeMap::from([(
                "work".to_owned(),
                Profile {
                    provider: Some("openrouter".into()),
                    google_api_key: Some("work-key".into()),
                    color: Some("#8FA6B8".into()),
                    ..Profile::default()
                },
            )]),
        };
        // The profile stands alone: personal's model and keys never leak
        // into it.
        let work = config.effective(Some("work"));
        assert_eq!(work.provider.as_deref(), Some("openrouter"));
        assert_eq!(work.google_api_key.as_deref(), Some("work-key"));
        assert_eq!(work.model, None);
        assert_eq!(work.color.as_deref(), Some("#8FA6B8"));

        // No flag: the stored active profile applies.
        let active = config.effective(None);
        assert_eq!(active.provider.as_deref(), Some("openrouter"));

        // An unknown name: the top-level section stands in (legacy
        // configs live there).
        let fallback = config.effective(Some("other"));
        assert_eq!(fallback.provider.as_deref(), Some("google"));
        assert_eq!(fallback.model.as_deref(), Some("personal-model"));
        assert_eq!(fallback.color, None);
    }

    #[test]
    fn set_for_profile_writes_the_named_section_or_the_default() {
        let mut config = Config {
            profile: Some("work".into()),
            ..Config::default()
        };
        config.set_for_profile(Some("work"), |p| p.google_api_key = Some("work-key".into()));
        // The active profile receives unflagged writes too.
        config.set_for_profile(None, |p| p.model = Some("m".into()));
        assert_eq!(
            config.profiles["work"].google_api_key.as_deref(),
            Some("work-key")
        );
        assert_eq!(config.profiles["work"].model.as_deref(), Some("m"));
        // The default section is untouched by profile writes.
        assert_eq!(config.default.google_api_key, None);
        // Without an active profile, writes land at the top level.
        config.profile = None;
        config.set_for_profile(None, |p| p.model = Some("top".into()));
        assert_eq!(config.default.model.as_deref(), Some("top"));
    }
}
