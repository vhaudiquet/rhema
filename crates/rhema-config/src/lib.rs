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
//!
//! A profile holds any number of named [`ProviderInstance`]s — two
//! llama.cpp servers, a work and a personal Copilot sub — with one of
//! them active (`provider` + `instance`). Files written before
//! instances existed carry one flat credential field per provider;
//! loading folds those into instances named after their provider.
//!
//! Environment variables (`RHEMA_PROVIDER`, `GEMINI_API_KEY`,
//! `GOOGLE_API_KEY`, `OPENROUTER_API_KEY`, `ANTHROPIC_API_KEY`,
//! `OPENAI_API_KEY`, `DEEPSEEK_API_KEY`, `GITHUB_COPILOT_TOKEN`) always
//! win over the stored value — a stored entry is the fallback, not the
//! override.

#![forbid(unsafe_code)]
#![deny(missing_docs)]

pub mod privacy;

use crate::privacy::{private_dir, private_write};
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

/// One logged-in provider instance: a named credential set for a single
/// provider kind. A profile may hold any number of them — two llama.cpp
/// servers, two OpenAI-compatible endpoints, a work and a personal
/// Copilot sub — keyed by a user-chosen name; the instance's `provider`
/// says which adapter speaks for it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "snake_case")]
pub struct ProviderInstance {
    /// Which adapter serves this instance: `google`, `llama-cpp`,
    /// `openrouter`, `anthropic`, `openai`, `deepseek`, or `copilot`.
    pub provider: String,
    /// The API key (the bearer key for a llama.cpp server), when the
    /// instance needs one. Environment variables still win at use time.
    pub api_key: Option<String>,
    /// The server base URL: the llama.cpp address, or an OpenAI-compatible
    /// endpoint's base. Unset means the provider's own default.
    pub base_url: Option<String>,
}

/// One named set of provider settings. Profiles are isolated: work never
/// inherits personal's model or keys, and vice versa.
///
/// Credentials live in [`Profile::instances`] — named
/// [`ProviderInstance`]s. The pre-instance flat key fields below are
/// legacy: [`Config::load_from`] folds them into instances and clears
/// them, and the per-provider accessors read the instance first, the
/// flat field second, so profiles built by hand (tests, older callers)
/// keep working.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "snake_case")]
pub struct Profile {
    /// Which provider serves turns: `google`, `llama-cpp`, `openrouter`,
    /// `anthropic`, `openai`, `deepseek`, or `copilot`.
    pub provider: Option<String>,
    /// The name of the active instance in [`Profile::instances`] — the
    /// one whose backend serves turns. Must name an instance of
    /// `provider` to count.
    pub instance: Option<String>,
    /// The logged-in provider instances, keyed by a user-chosen name.
    pub instances: BTreeMap<String, ProviderInstance>,
    /// The last-used model, when one was picked.
    pub model: Option<String>,
    /// Legacy Google API key; migrated into an instance on load.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub google_api_key: Option<String>,
    /// Legacy llama.cpp server base URL; migrated into an instance on
    /// load.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub llama_cpp_url: Option<String>,
    /// Legacy llama.cpp bearer key; migrated into an instance on load.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub llama_cpp_api_key: Option<String>,
    /// Legacy OpenRouter API key; migrated into an instance on load.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub openrouter_api_key: Option<String>,
    /// Legacy Anthropic API key; migrated into an instance on load.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub anthropic_api_key: Option<String>,
    /// Legacy OpenAI API key; migrated into an instance on load.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub openai_api_key: Option<String>,
    /// Legacy DeepSeek API key; migrated into an instance on load.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deepseek_api_key: Option<String>,
    /// Legacy GitHub OAuth token the Copilot adapter runs on — the
    /// device flow's long-lived output; migrated into an instance on
    /// load. The short-lived Copilot token it exchanges for never
    /// touches disk.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub copilot_api_key: Option<String>,
    /// The color this profile's name wears in the UI, as `#RRGGBB`.
    /// Unset means the UI picks a tone from the theme by profile name.
    pub color: Option<String>,
    /// How much reasoning the model should do: `low`, `medium`, or
    /// `high`. Unset means the provider's default stands. Anything
    /// unparseable reads as unset.
    pub thinking: Option<String>,
}

impl Profile {
    /// The provider this profile serves turns on: `RHEMA_PROVIDER`, then
    /// the stored `provider`. Whitespace-only values count as unset. The
    /// name is returned as given — unknown spellings are the consumer's
    /// to resolve or reject.
    pub fn provider_name(&self) -> Option<String> {
        resolve_key(
            |name| std::env::var(name).ok(),
            &["RHEMA_PROVIDER"],
            self.provider.clone(),
        )
    }

    /// The Google API key to use: `GEMINI_API_KEY`, then `GOOGLE_API_KEY`,
    /// then the stored key of the profile's Google instance (legacy flat
    /// field last). Whitespace-only values count as unset.
    pub fn google_api_key(&self) -> Option<String> {
        self.provider_key("google", self.google_api_key.clone())
    }

    /// The llama.cpp server URL to use: `LLAMA_CPP_URL`, then the llama
    /// instance's URL, then `http://localhost:8080`.
    pub fn llama_cpp_url(&self) -> String {
        resolve_key(
            |name| std::env::var(name).ok(),
            &["LLAMA_CPP_URL"],
            self.llama_url().or(self.llama_cpp_url.clone()),
        )
        .unwrap_or_else(|| "http://localhost:8080".to_owned())
    }

    /// The llama.cpp bearer key to use: `LLAMA_CPP_API_KEY`, then the
    /// llama instance's key (legacy flat field last); `None` when the
    /// server is open.
    pub fn llama_cpp_api_key(&self) -> Option<String> {
        self.provider_key("llama-cpp", self.llama_cpp_api_key.clone())
    }

    /// The OpenRouter key to use: `OPENROUTER_API_KEY`, then the
    /// OpenRouter instance's key (legacy flat field last).
    pub fn openrouter_api_key(&self) -> Option<String> {
        self.provider_key("openrouter", self.openrouter_api_key.clone())
    }

    /// The Anthropic key to use: `ANTHROPIC_API_KEY`, then the Anthropic
    /// instance's key (legacy flat field last).
    pub fn anthropic_api_key(&self) -> Option<String> {
        self.provider_key("anthropic", self.anthropic_api_key.clone())
    }

    /// The OpenAI key to use: `OPENAI_API_KEY`, then the OpenAI
    /// instance's key (legacy flat field last).
    pub fn openai_api_key(&self) -> Option<String> {
        self.provider_key("openai", self.openai_api_key.clone())
    }

    /// The DeepSeek key to use: `DEEPSEEK_API_KEY`, then the DeepSeek
    /// instance's key (legacy flat field last).
    pub fn deepseek_api_key(&self) -> Option<String> {
        self.provider_key("deepseek", self.deepseek_api_key.clone())
    }

    /// The GitHub token the Copilot adapter runs on:
    /// `GITHUB_COPILOT_TOKEN`, then the Copilot instance's token (legacy
    /// flat field last).
    pub fn copilot_api_key(&self) -> Option<String> {
        self.provider_key("copilot", self.copilot_api_key.clone())
    }

    /// The OpenAI-compatible base URL of the profile's OpenAI instance:
    /// the stored base, or `https://api.openai.com/v1`. `None` when the
    /// profile holds no OpenAI instance.
    pub fn openai_base_url(&self) -> Option<String> {
        self.instance_for_provider("openai")?;
        Some(
            self.instance_url_for("openai")
                .unwrap_or_else(|| "https://api.openai.com/v1".to_owned()),
        )
    }

    /// The profile's thinking level, when set and parseable.
    pub fn thinking_effort(&self) -> Option<ThinkingEffort> {
        ThinkingEffort::parse(self.thinking.as_deref()?)
    }

    /// Whether this profile can build a backend: it names a provider and
    /// carries the credential that provider needs (a llama.cpp server
    /// works keyless; the rest need a key, env included).
    pub fn has_credentials(&self) -> bool {
        match self.provider.as_deref() {
            Some("llama-cpp") => true,
            Some("openrouter") => self.openrouter_api_key().is_some(),
            Some("anthropic") => self.anthropic_api_key().is_some(),
            Some("openai") => self.openai_api_key().is_some(),
            Some("deepseek") => self.deepseek_api_key().is_some(),
            Some("copilot") => self.copilot_api_key().is_some(),
            _ => self.google_api_key().is_some(),
        }
    }

    /// The stored instances, sorted by name.
    pub fn instance_entries(&self) -> Vec<(&str, &ProviderInstance)> {
        self.instances
            .iter()
            .map(|(name, instance)| (name.as_str(), instance))
            .collect()
    }

    /// The instance that serves `provider`: the active one when the
    /// profile's active instance is of that provider, else the first
    /// stored instance of it, else `None`.
    pub fn instance_for_provider(&self, provider: &str) -> Option<(&str, &ProviderInstance)> {
        if self.provider.as_deref() == Some(provider) {
            if let Some((name, instance)) = self.active_instance() {
                return Some((name, instance));
            }
        }
        self.instances
            .iter()
            .find(|(_, instance)| instance.provider == provider)
            .map(|(name, instance)| (name.as_str(), instance))
    }

    /// The active instance: the one `instance` names when it exists and
    /// serves `provider`, else the first instance of `provider`, else
    /// `None`.
    pub fn active_instance(&self) -> Option<(&str, &ProviderInstance)> {
        let provider = self.provider.as_deref()?;
        if let Some(name) = self.instance.as_deref() {
            if let Some(instance) = self.instances.get(name) {
                if instance.provider == provider {
                    return Some((name, instance));
                }
            }
        }
        self.instances
            .iter()
            .find(|(_, instance)| instance.provider == provider)
            .map(|(name, instance)| (name.as_str(), instance))
    }

    /// Inserts or replaces the named instance. The caller keeps
    /// `provider`/`instance` coherent — a new active instance must be
    /// named in `instance` and its kind in `provider`.
    pub fn upsert_instance(&mut self, name: &str, instance: ProviderInstance) {
        self.instances.insert(name.to_owned(), instance);
    }

    /// Removes the named instance, returning it. When it was the active
    /// one, `instance` is cleared; `provider` stays until the caller
    /// names a replacement.
    pub fn remove_instance(&mut self, name: &str) -> Option<ProviderInstance> {
        let removed = self.instances.remove(name);
        if removed.is_some() && self.instance.as_deref() == Some(name) {
            self.instance = None;
        }
        removed
    }

    /// The stored key for `provider` without the environment tier: the
    /// matching instance's key, else the legacy flat field. Callers that
    /// run their own environment precedence (the claw's discovery) read
    /// this instead of the env-aware accessors.
    pub fn stored_provider_key(&self, provider: &str) -> Option<String> {
        match self.instance_for_provider(provider) {
            Some((_, instance)) => instance
                .api_key
                .clone()
                .map(|key| key.trim().to_owned())
                .filter(|key| !key.is_empty()),
            None => match provider {
                "google" => self.google_api_key.clone(),
                "llama-cpp" => self.llama_cpp_api_key.clone(),
                "openrouter" => self.openrouter_api_key.clone(),
                "anthropic" => self.anthropic_api_key.clone(),
                "openai" => self.openai_api_key.clone(),
                "deepseek" => self.deepseek_api_key.clone(),
                "copilot" => self.copilot_api_key.clone(),
                _ => None,
            }
            .map(|key| key.trim().to_owned())
            .filter(|key| !key.is_empty()),
        }
    }

    /// The key an instance serves: the provider's environment variables
    /// first, then the stored key. Whitespace-only values count as
    /// unset.
    pub fn instance_api_key(&self, instance: &ProviderInstance) -> Option<String> {
        resolve_key(
            |name| std::env::var(name).ok(),
            provider_env_names(&instance.provider),
            instance.api_key.clone(),
        )
    }

    /// The base URL an instance serves: `LLAMA_CPP_URL` over the stored
    /// URL for llama.cpp; the stored base for OpenAI-compatible
    /// instances (the caller defaults OpenAI itself, so its own default
    /// stays in one place); `None` for key-only providers.
    pub fn instance_url(&self, instance: &ProviderInstance) -> Option<String> {
        match instance.provider.as_str() {
            "llama-cpp" => Some(
                resolve_key(
                    |name| std::env::var(name).ok(),
                    &["LLAMA_CPP_URL"],
                    instance.base_url.clone(),
                )
                .unwrap_or_else(|| "http://localhost:8080".to_owned()),
            ),
            "openai" => instance
                .base_url
                .clone()
                .filter(|url| !url.trim().is_empty()),
            _ => None,
        }
    }

    /// The instance URL for `provider`, via [`Profile::instance_url`].
    fn instance_url_for(&self, provider: &str) -> Option<String> {
        let (_, instance) = self.instance_for_provider(provider)?;
        self.instance_url(instance)
    }

    /// The llama instance's URL, without the legacy fallback default.
    fn llama_url(&self) -> Option<String> {
        self.instance_url_for("llama-cpp")
    }

    /// The key `provider` serves: the matching instance's key (env
    /// included), or `fallback` — the legacy flat field — when the
    /// profile holds no instance of it.
    fn provider_key(&self, provider: &str, fallback: Option<String>) -> Option<String> {
        match self.instance_for_provider(provider) {
            Some((_, instance)) => self.instance_api_key(instance),
            None => resolve_key(
                |name| std::env::var(name).ok(),
                provider_env_names(provider),
                fallback,
            ),
        }
    }

    /// Folds the legacy flat credential fields into named instances (one
    /// per provider, named after it) and clears them, keeping `provider`
    /// as the active instance. Only fires on instance-less profiles, so
    /// a partially-new file is never rewritten by guesswork.
    fn migrate_legacy_credentials(&mut self) {
        if !self.instances.is_empty() {
            return;
        }
        let legacy = [
            ("google", self.google_api_key.take(), None),
            ("openrouter", self.openrouter_api_key.take(), None),
            ("anthropic", self.anthropic_api_key.take(), None),
            ("openai", self.openai_api_key.take(), None),
            ("deepseek", self.deepseek_api_key.take(), None),
            ("copilot", self.copilot_api_key.take(), None),
        ];
        for (provider, key, base) in legacy {
            if let Some(api_key) = key {
                self.instances.insert(
                    provider.to_owned(),
                    ProviderInstance {
                        provider: provider.to_owned(),
                        api_key: Some(api_key),
                        base_url: base,
                    },
                );
            }
        }
        let llama_key = self.llama_cpp_api_key.take();
        let llama_url = self.llama_cpp_url.take();
        if llama_key.is_some() || llama_url.is_some() {
            self.instances.insert(
                "llama-cpp".to_owned(),
                ProviderInstance {
                    provider: "llama-cpp".to_owned(),
                    api_key: llama_key,
                    base_url: llama_url,
                },
            );
        }
        if self.instance.is_none() {
            self.instance = self
                .provider
                .clone()
                .filter(|provider| self.instances.contains_key(provider));
        }
    }
}

/// The environment variables whose values override a stored key for
/// `provider`, in precedence order.
fn provider_env_names(provider: &str) -> &'static [&'static str] {
    match provider {
        "google" => &["GEMINI_API_KEY", "GOOGLE_API_KEY"],
        "llama-cpp" => &["LLAMA_CPP_API_KEY"],
        "openrouter" => &["OPENROUTER_API_KEY"],
        "anthropic" => &["ANTHROPIC_API_KEY"],
        "openai" => &["OPENAI_API_KEY"],
        "deepseek" => &["DEEPSEEK_API_KEY"],
        "copilot" => &["GITHUB_COPILOT_TOKEN"],
        _ => &[],
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
    /// The footer's icon flavor: `unicode` (the default) or `nerd` for
    /// Nerd Font glyphs. Anything unparseable reads as the default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icons: Option<String>,
    /// The color theme: `dark` (the default) or `light`. Validated by
    /// the consumer, not here — anything unparseable reads as the
    /// default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub theme: Option<String>,
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
            icons: None,
            theme: None,
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
    /// says "no provider; use /login").
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

    /// Persists the config, creating the directory `0700` and the file
    /// `0600` on Unix because it holds credentials. Both modes are
    /// re-asserted on every save, so a config that predates the rule —
    /// or was widened elsewhere — tightens again.
    pub fn save(&self) -> Result<(), ConfigError> {
        let dir = Self::dir()?;
        private_dir(&dir)?;
        Self::save_to(&dir.join("config.yaml"), self)
    }

    /// [`Config::load`] against an explicit file (missing file is the
    /// default config). Profiles carrying the legacy flat credential
    /// fields are migrated into named instances, so a caller saving the
    /// loaded config back out writes the new shape.
    pub fn load_from(path: &impl AsRef<Path>) -> Result<Config, ConfigError> {
        let raw = match std::fs::read_to_string(path) {
            Ok(raw) => raw,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Config::default());
            }
            Err(err) => return Err(err.into()),
        };
        let mut config: Config = serde_yaml::from_str(&raw)?;
        for profile in config.profiles.values_mut() {
            profile.migrate_legacy_credentials();
        }
        Ok(config)
    }

    /// [`Config::save`] to an explicit file, with the same `0600`
    /// handling.
    pub fn save_to(path: &impl AsRef<Path>, config: &Config) -> Result<(), ConfigError> {
        let yaml = serde_yaml::to_string(config)?;
        private_write(path.as_ref(), yaml.as_bytes())?;
        Ok(())
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
            deepseek_api_key: provider.eq("deepseek").then(|| "k".to_owned()),
            copilot_api_key: provider.eq("copilot").then(|| "k".to_owned()),
            ..Profile::default()
        };
        // A stored key makes the profile usable...
        assert!(keyed("anthropic").has_credentials());
        assert!(keyed("openai").has_credentials());
        assert!(keyed("deepseek").has_credentials());
        assert!(keyed("copilot").has_credentials());
        // ...and each provider needs exactly its own key.
        assert!(keyed("anthropic").openai_api_key.is_none());
        assert!(keyed("openai").anthropic_api_key.is_none());
        // No stored key and no env var: not usable. (Machines running the
        // suite with these env vars exported would see the env key win,
        // per the convention above.)
        assert!(!bare("anthropic").has_credentials());
        assert!(!bare("openai").has_credentials());
        assert!(!bare("deepseek").has_credentials());
        assert!(!bare("copilot").has_credentials());
    }

    #[test]
    fn the_provider_name_follows_the_same_env_wins_convention() {
        // A blank RHEMA_PROVIDER counts as unset: the stored provider
        // serves. The real env is only read when the suite does not
        // export the var itself; the pure resolver carries the rest of
        // the env-wins coverage (a test setting it would race the run).
        let blank = |name: &str| (name == "RHEMA_PROVIDER").then(String::new);
        assert_eq!(
            resolve_key(blank, &["RHEMA_PROVIDER"], Some("openrouter".into())),
            Some("openrouter".into())
        );
        assert_eq!(
            resolve_key(
                |_| Some("openrouter".into()),
                &["RHEMA_PROVIDER"],
                Some("google".into())
            ),
            Some("openrouter".into())
        );
        assert_eq!(resolve_key(|_| None, &["RHEMA_PROVIDER"], None), None);
        if std::env::var_os("RHEMA_PROVIDER").is_none() {
            let profile = Profile {
                provider: Some("openrouter".into()),
                ..Profile::default()
            };
            assert_eq!(profile.provider_name().as_deref(), Some("openrouter"));
            assert_eq!(Profile::default().provider_name(), None);
        }
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
                    instance: Some("home server".into()),
                    instances: BTreeMap::from([
                        (
                            "home server".to_owned(),
                            ProviderInstance {
                                provider: "llama-cpp".into(),
                                api_key: None,
                                base_url: Some("http://localhost:8080".into()),
                            },
                        ),
                        (
                            "anthropic".to_owned(),
                            ProviderInstance {
                                provider: "anthropic".into(),
                                api_key: Some("sk-ant-123".into()),
                                base_url: None,
                            },
                        ),
                        (
                            "copilot".to_owned(),
                            ProviderInstance {
                                provider: "copilot".into(),
                                api_key: Some("gho-123".into()),
                                base_url: None,
                            },
                        ),
                        (
                            "deepseek".to_owned(),
                            ProviderInstance {
                                provider: "deepseek".into(),
                                api_key: Some("sk-dsk-123".into()),
                                base_url: None,
                            },
                        ),
                    ]),
                    model: Some("gemini-3.8-flash".into()),
                    color: None,
                    thinking: Some("low".into()),
                    ..Profile::default()
                },
            )]),
            icons: None,
            theme: Some("light".into()),
            last_profile_by_workspace: BTreeMap::new(),
        };
        Config::save_to(&path, &config).unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.contains("instances:"), "yaml on disk: {raw}");
        assert!(raw.contains("sk-ant-123"), "yaml on disk: {raw}");
        assert!(raw.contains("gho-123"), "yaml on disk: {raw}");
        assert!(raw.contains("sk-dsk-123"), "yaml on disk: {raw}");
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
    fn a_legacy_config_migrates_its_flat_keys_into_named_instances() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.yaml");
        std::fs::write(
            &path,
            "\
profiles:
  personal:
    provider: openai
    model: gpt-5
    google_api_key: g-key
    openai_api_key: o-key
    llama_cpp_url: http://box:8080
    llama_cpp_api_key: bearer
  work:
    provider: copilot
    copilot_api_key: gho-1
",
        )
        .unwrap();
        let loaded = Config::load_from(&path).unwrap();

        let personal = &loaded.profiles["personal"];
        assert_eq!(personal.provider.as_deref(), Some("openai"));
        assert_eq!(personal.instance.as_deref(), Some("openai"));
        let entries: Vec<(String, Option<String>)> = personal
            .instance_entries()
            .into_iter()
            .map(|(name, i)| (name.to_owned(), i.base_url.clone().or(i.api_key.clone())))
            .collect();
        assert_eq!(
            entries,
            vec![
                ("google".into(), Some("g-key".into())),
                ("llama-cpp".into(), Some("http://box:8080".into())),
                ("openai".into(), Some("o-key".into())),
            ]
        );
        // The llama bearer rode along with the URL.
        assert_eq!(
            personal.instances["llama-cpp"].api_key.as_deref(),
            Some("bearer")
        );
        // The flat fields are gone: the next save writes the new shape.
        let saved = dir.path().join("rewritten.yaml");
        Config::save_to(&saved, &loaded).unwrap();
        let raw = std::fs::read_to_string(&saved).unwrap();
        assert!(!raw.contains("google_api_key"), "yaml on disk: {raw}");
        assert!(!raw.contains("llama_cpp_url"), "yaml on disk: {raw}");
        // The legacy accessors read the migrated instances back.
        assert_eq!(personal.google_api_key().as_deref(), Some("g-key"));
        assert_eq!(personal.openai_api_key().as_deref(), Some("o-key"));
        assert_eq!(personal.llama_cpp_url(), "http://box:8080");
        assert_eq!(personal.llama_cpp_api_key().as_deref(), Some("bearer"));

        // work migrates too, and keeps its own section.
        let work = &loaded.profiles["work"];
        assert_eq!(work.instance.as_deref(), Some("copilot"));
        assert_eq!(work.copilot_api_key().as_deref(), Some("gho-1"));
    }

    #[test]
    fn the_active_instance_resolves_through_instance_name_then_provider() {
        let profile = Profile {
            provider: Some("llama-cpp".into()),
            instance: Some("gpu box".into()),
            instances: BTreeMap::from([
                (
                    "gpu box".to_owned(),
                    ProviderInstance {
                        provider: "llama-cpp".into(),
                        api_key: None,
                        base_url: Some("http://gpu:8080".into()),
                    },
                ),
                (
                    "home".to_owned(),
                    ProviderInstance {
                        provider: "llama-cpp".into(),
                        api_key: None,
                        base_url: Some("http://home:8080".into()),
                    },
                ),
            ]),
            ..Profile::default()
        };
        let (name, instance) = profile.active_instance().unwrap();
        assert_eq!(name, "gpu box");
        assert_eq!(
            profile.instance_url(instance).as_deref(),
            Some("http://gpu:8080")
        );
        // A dangling `instance` name falls back to the first instance of
        // the active provider.
        let mut dangling = profile.clone();
        dangling.instance = Some("nope".into());
        assert_eq!(dangling.active_instance().unwrap().0, "gpu box");
        // The same provider with a different active provider set: the
        // first stored llama instance serves.
        let mut elsewhere = profile.clone();
        elsewhere.provider = Some("openai".into());
        elsewhere.instance = Some("gpu box".into());
        assert_eq!(elsewhere.active_instance(), None);
        assert_eq!(
            elsewhere.instance_for_provider("llama-cpp").unwrap().0,
            "gpu box"
        );

        // Removing the active instance clears the pointer, not the
        // provider.
        let mut removed = profile.clone();
        assert!(removed.remove_instance("gpu box").is_some());
        assert_eq!(removed.instance, None);
        assert_eq!(removed.provider.as_deref(), Some("llama-cpp"));
        assert_eq!(removed.active_instance().unwrap().0, "home");
        // Removing a name that is not there removes nothing.
        assert!(removed.remove_instance("gpu box").is_none());
    }

    #[test]
    fn two_instances_of_one_provider_hold_their_own_keys() {
        let mut profile = Profile {
            provider: Some("openai".into()),
            instance: Some("personal".into()),
            ..Profile::default()
        };
        profile.upsert_instance(
            "personal",
            ProviderInstance {
                provider: "openai".into(),
                api_key: Some("sk-personal".into()),
                base_url: None,
            },
        );
        profile.upsert_instance(
            "proxy",
            ProviderInstance {
                provider: "openai".into(),
                api_key: Some("sk-proxy".into()),
                base_url: Some("http://localhost:1234/v1".into()),
            },
        );
        assert_eq!(profile.openai_api_key().as_deref(), Some("sk-personal"));
        assert_eq!(
            profile.openai_base_url().as_deref(),
            Some("https://api.openai.com/v1")
        );
        // Activating the proxy instance moves both the key and the base.
        profile.instance = Some("proxy".into());
        assert_eq!(profile.openai_api_key().as_deref(), Some("sk-proxy"));
        assert_eq!(
            profile.openai_base_url().as_deref(),
            Some("http://localhost:1234/v1")
        );
        assert!(profile.has_credentials());
        // Pointing at a missing name falls back to the first openai
        // instance rather than failing open.
        profile.instance = Some("nope".into());
        assert_eq!(profile.openai_api_key().as_deref(), Some("sk-personal"));

        // The env-free read serves callers with their own precedence.
        profile.instance = Some("proxy".into());
        assert_eq!(
            profile.stored_provider_key("openai").as_deref(),
            Some("sk-proxy")
        );
        // Legacy flat fields back it when no instance matches.
        let legacy = Profile {
            openai_api_key: Some(" sk-flat ".into()),
            ..Profile::default()
        };
        assert_eq!(
            legacy.stored_provider_key("openai").as_deref(),
            Some("sk-flat")
        );
    #[cfg(unix)]
    fn a_widened_config_file_tightens_on_the_next_save() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.yaml");
        std::fs::write(&path, "stale").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        Config::save_to(&path, &Config::default()).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "credentials file must be 0600");
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
            icons: None,
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
