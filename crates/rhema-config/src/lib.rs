//! Persistent rhema settings: the registry of named profiles.
//!
//! The file lives at `$XDG_CONFIG_HOME/rhema/config.yaml`, written with
//! `0600` permissions since it holds credentials. Which profile a session
//! runs under is chosen per invocation (`--profile`) or per session
//! (`/profile`) — it is never stored as a global switch, so parallel
//! instances (work and personal side by side) never fight over one
//! toggle. What is kept, per workspace, is the last profile that served
//! it a turn — a directory reopens on the profile it last used, never
//! overriding an explicit flag.
//! Environment variables (`GEMINI_API_KEY`, `GOOGLE_API_KEY`,
//! `OPENROUTER_API_KEY`, `ANTHROPIC_API_KEY`, `OPENAI_API_KEY`) always
//! win over a stored key — a stored key is the fallback, not the
//! override.

#![forbid(unsafe_code)]
#![deny(missing_docs)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use rhema_core::request::ThinkingEffort;
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

/// One named set of provider settings. Profiles are isolated: work never
/// inherits personal's model or keys, and vice versa.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "snake_case")]
pub struct Profile {
    /// Which provider serves turns: `google`, `llama-cpp`, `openrouter`,
    /// `anthropic`, or `openai`.
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
    /// The Anthropic API key entered at login, when one is stored.
    pub anthropic_api_key: Option<String>,
    /// The OpenAI API key entered at login, when one is stored.
    pub openai_api_key: Option<String>,
    /// The color this profile's name wears in the UI, as `#RRGGBB`.
    /// Unset means the UI picks a tone from the theme by profile name.
    pub color: Option<String>,
    /// How much reasoning the model should do: `low`, `medium`, or
    /// `high`. Unset means the provider's default stands. Anything
    /// unparseable reads as unset.
    pub thinking: Option<String>,
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

    /// The Anthropic key to use: `ANTHROPIC_API_KEY`, then the stored
    /// key.
    pub fn anthropic_api_key(&self) -> Option<String> {
        resolve_key(
            |name| std::env::var(name).ok(),
            &["ANTHROPIC_API_KEY"],
            self.anthropic_api_key.clone(),
        )
    }

    /// The OpenAI key to use: `OPENAI_API_KEY`, then the stored key.
    pub fn openai_api_key(&self) -> Option<String> {
        resolve_key(
            |name| std::env::var(name).ok(),
            &["OPENAI_API_KEY"],
            self.openai_api_key.clone(),
        )
    }

    /// The profile's thinking level, when set and parseable.
    pub fn thinking_effort(&self) -> Option<ThinkingEffort> {
        ThinkingEffort::parse(self.thinking.as_deref()?)
    }

    /// Whether this profile can build a backend: it names a provider and
    /// carries the credential that provider needs (a llama.cpp server
    /// works keyless; google, openrouter, anthropic, and openai need a
    /// key, env included).
    pub fn has_credentials(&self) -> bool {
        match self.provider.as_deref() {
            Some("llama-cpp") => true,
            Some("openrouter") => self.openrouter_api_key().is_some(),
            Some("anthropic") => self.anthropic_api_key().is_some(),
            Some("openai") => self.openai_api_key().is_some(),
            _ => self.google_api_key().is_some(),
        }
    }
}

/// The persisted settings: the registry of named profiles.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "snake_case")]
pub struct Config {
    /// Named profiles; each stands alone. Fresh setups ship `personal`
    /// and `work`.
    #[serde(
        default = "default_profiles",
        skip_serializing_if = "BTreeMap::is_empty"
    )]
    pub profiles: BTreeMap<String, Profile>,
    /// The last profile that served each workspace a turn, keyed by the
    /// workspace's absolute path. Startup reopens a directory on its
    /// last-used profile — a per-directory memory, never a global
    /// switch, so parallel instances in different directories stay
    /// independent.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub last_profile_by_workspace: BTreeMap<String, String>,
}

/// The profiles on fresh setups and files that omit the key.
fn default_profiles() -> BTreeMap<String, Profile> {
    BTreeMap::from([
        ("personal".to_owned(), Profile::default()),
        ("work".to_owned(), Profile::default()),
    ])
}

impl Default for Config {
    /// Fresh setups ship with the two everyday profiles. Loaded files
    /// missing the key get the same treatment via the field-level serde
    /// default.
    fn default() -> Self {
        Self {
            profiles: default_profiles(),
            last_profile_by_workspace: BTreeMap::new(),
        }
    }
}

impl Config {
    /// The settings that apply for `flag` (or `personal` when absent).
    ///
    /// ISOLATION: a profile's section is authoritative — work never
    /// inherits personal's model or keys. An unknown name resolves to an
    /// empty profile; the caller decides how to present that (the TUI
    /// says "no provider — use /login").
    pub fn effective(&self, flag: Option<&str>) -> Profile {
        self.profiles
            .get(flag.unwrap_or("personal"))
            .cloned()
            .unwrap_or_default()
    }

    /// Mutates the section that writes land in for `flag` (or `personal`
    /// when absent): the named profile, created when new.
    pub fn set_for_profile(&mut self, flag: Option<&str>, apply: impl FnOnce(&mut Profile)) {
        let name = flag
            .map(str::to_owned)
            .unwrap_or_else(|| "personal".to_owned());
        apply(self.profiles.entry(name).or_default());
    }

    /// The profile `workspace` last served a turn under, when one was
    /// recorded. The name is returned as stored — the caller decides
    /// whether that profile still exists and can serve turns.
    pub fn last_profile_in(&self, workspace: &Path) -> Option<String> {
        self.last_profile_by_workspace
            .get(&workspace.to_string_lossy().into_owned())
            .cloned()
    }

    /// Records `profile` as the last one `workspace` served a turn
    /// under. A no-op when the map already holds the name, so a turn per
    /// keystroke never rewrites the file.
    pub fn note_profile_in(&mut self, workspace: &Path, profile: &str) {
        let changed = self.last_profile_in(workspace).as_deref() != Some(profile);
        if changed {
            self.last_profile_by_workspace
                .insert(workspace.to_string_lossy().into_owned(), profile.to_owned());
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

    /// The global skills directory (`<dir>/skills`), where user-wide
    /// skills live; a workspace can add more under its own root.
    pub fn skills_dir() -> Result<PathBuf, ConfigError> {
        Ok(Self::dir()?.join("skills"))
    }

    /// The system prompt file (`<dir>/SYSTEM.md`), the editable base
    /// prompt seeded with the built-in default on first run.
    pub fn system_prompt_path() -> Result<PathBuf, ConfigError> {
        Ok(Self::dir()?.join("SYSTEM.md"))
    }

    /// Loads the config; a missing file is the default config, not an
    /// error.
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
    fn anthropic_and_openai_keys_follow_the_same_env_wins_convention() {
        let env = |name: &str| match name {
            "ANTHROPIC_API_KEY" => Some("anthropic-from-env".into()),
            "OPENAI_API_KEY" => Some("  ".into()),
            _ => None,
        };
        assert_eq!(
            resolve_key(env, &["ANTHROPIC_API_KEY"], Some("stored".into())),
            Some("anthropic-from-env".into())
        );
        // A blank env var counts as unset: the stored key serves.
        assert_eq!(
            resolve_key(env, &["OPENAI_API_KEY"], Some("stored".into())),
            Some("stored".into())
        );
    }

    #[test]
    fn anthropic_and_openai_profiles_need_their_own_key_to_serve() {
        let bare = |provider: &str| Profile {
            provider: Some(provider.to_owned()),
            ..Profile::default()
        };
        let keyed = |provider: &str| Profile {
            provider: Some(provider.to_owned()),
            anthropic_api_key: provider.eq("anthropic").then(|| "k".to_owned()),
            openai_api_key: provider.eq("openai").then(|| "k".to_owned()),
            ..Profile::default()
        };
        // A stored key makes the profile usable...
        assert!(keyed("anthropic").has_credentials());
        assert!(keyed("openai").has_credentials());
        // ...and each provider needs exactly its own key.
        assert!(keyed("anthropic").openai_api_key.is_none());
        assert!(keyed("openai").anthropic_api_key.is_none());
        // No stored key and no env var: not usable. (Machines running the
        // suite with these env vars exported would see the env key win,
        // per the convention above.)
        assert!(!bare("anthropic").has_credentials());
        assert!(!bare("openai").has_credentials());
    }

    #[test]
    fn config_round_trips_through_disk_as_yaml() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.yaml");
        let config = Config {
            profiles: BTreeMap::from([(
                "personal".to_owned(),
                Profile {
                    provider: Some("llama-cpp".into()),
                    google_api_key: Some("key-123".into()),
                    model: Some("gemini-3.8-flash".into()),
                    llama_cpp_url: Some("http://localhost:8080".into()),
                    llama_cpp_api_key: None,
                    openrouter_api_key: None,
                    anthropic_api_key: Some("sk-ant-123".into()),
                    openai_api_key: None,
                    color: None,
                    thinking: Some("low".into()),
                },
            )]),
            last_profile_by_workspace: BTreeMap::new(),
        };
        Config::save_to(&path, &config).unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.contains("google_api_key:"), "yaml on disk: {raw}");
        assert!(raw.contains("anthropic_api_key:"), "yaml on disk: {raw}");
        assert!(!raw.contains('{'), "not json: {raw}");
        let loaded = Config::load_from(&path).unwrap();
        assert_eq!(loaded, config);

        // The thinking level parses; a junk value reads as unset.
        let mut config = config;
        assert_eq!(
            config.profiles["personal"].thinking_effort(),
            Some(ThinkingEffort::Low)
        );
        config.profiles.get_mut("personal").unwrap().thinking = Some("maximum".into());
        assert_eq!(config.profiles["personal"].thinking_effort(), None);

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
        assert!(config.profiles.contains_key("personal"));
        assert!(config.profiles.contains_key("work"));
        // A loaded file missing the key gets the same seeding.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.yaml");
        std::fs::write(&path, "model: m\n").unwrap();
        let loaded = Config::load_from(&path).unwrap();
        assert!(loaded.profiles.contains_key("work"));
        // The stray top-level key is ignored: profiles are the only shape.
        assert!(loaded.profiles["personal"].model.is_none());
    }

    #[test]
    fn profiles_are_isolated_from_each_other() {
        let config = Config {
            profiles: BTreeMap::from([
                (
                    "personal".to_owned(),
                    Profile {
                        provider: Some("google".into()),
                        google_api_key: Some("personal-key".into()),
                        model: Some("personal-model".into()),
                        ..Profile::default()
                    },
                ),
                (
                    "work".to_owned(),
                    Profile {
                        provider: Some("openrouter".into()),
                        google_api_key: Some("work-key".into()),
                        color: Some("#8FA6B8".into()),
                        ..Profile::default()
                    },
                ),
            ]),
            last_profile_by_workspace: BTreeMap::new(),
        };
        let work = config.effective(Some("work"));
        assert_eq!(work.provider.as_deref(), Some("openrouter"));
        assert_eq!(work.google_api_key.as_deref(), Some("work-key"));
        assert_eq!(work.model, None);
        assert_eq!(work.color.as_deref(), Some("#8FA6B8"));

        // personal's fields never leak into work, and vice versa.
        let personal = config.effective(Some("personal"));
        assert_eq!(personal.google_api_key.as_deref(), Some("personal-key"));
        assert_eq!(personal.model.as_deref(), Some("personal-model"));

        // An unknown name resolves to an empty profile: nothing leaks in.
        assert_eq!(config.effective(Some("other")), Profile::default());
    }

    #[test]
    fn set_for_profile_writes_the_named_section() {
        let mut config = Config::default();
        config.set_for_profile(Some("work"), |p| p.google_api_key = Some("work-key".into()));
        // Unflagged writes land on personal, the default profile.
        config.set_for_profile(None, |p| p.model = Some("m".into()));
        assert_eq!(
            config.profiles["work"].google_api_key.as_deref(),
            Some("work-key")
        );
        assert_eq!(config.profiles["personal"].model.as_deref(), Some("m"));
        // work's section is untouched by personal's writes.
        assert_eq!(config.profiles["work"].model, None);
    }

    #[test]
    fn workspaces_remember_their_last_profile_and_survive_a_round_trip() {
        let mut config = Config::default();
        assert_eq!(config.last_profile_in(Path::new("/home/u/proj")), None);

        config.note_profile_in(Path::new("/home/u/proj"), "work");
        assert_eq!(
            config.last_profile_in(Path::new("/home/u/proj")).as_deref(),
            Some("work")
        );
        // Another directory stays untouched: the memory is per workspace.
        assert_eq!(config.last_profile_in(Path::new("/home/u/other")), None);

        config.note_profile_in(Path::new("/home/u/proj"), "personal");
        assert_eq!(
            config.last_profile_in(Path::new("/home/u/proj")).as_deref(),
            Some("personal")
        );

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.yaml");
        Config::save_to(&path, &config).unwrap();
        let loaded = Config::load_from(&path).unwrap();
        assert_eq!(loaded, config);
        assert_eq!(
            loaded.last_profile_in(Path::new("/home/u/proj")).as_deref(),
            Some("personal")
        );
    }
}
