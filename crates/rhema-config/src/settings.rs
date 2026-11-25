//! The framework's own settings section and the whole-document round
//! trip a co-locating frontend needs.

use crate::error::ConfigError;
use crate::instances::migrate_instance_key;
use crate::privacy::{private_dir, private_write_atomic};
use crate::profile::Profile;
use crate::secrets::{LegacySecretStore, SecretStore};
use crate::store::{Store, YamlStore};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use serde::{Deserialize, Serialize};

/// The framework's own section of the settings: what any frontend
/// built on rhema must be able to store and read, whatever document
/// it co-locates them in. Everything beyond this, themes, palettes,
/// onboarding state, per-workspace reopen memory, is the frontend's
/// own section, in the frontend's own file.
///
/// Unknown keys are rejected: a typo'd key must not silently read as
/// its default. A frontend embedding the section into a bigger
/// document inherits the same strictness for these keys, a typo in
/// the framework's vocabulary is loud wherever it lands.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "snake_case", deny_unknown_fields)]
pub struct Settings {
    /// Named profiles; each stands alone. Fresh setups ship `personal`
    /// and `work`.
    #[serde(
        default = "default_profiles",
        skip_serializing_if = "BTreeMap::is_empty"
    )]
    pub profiles: BTreeMap<String, Profile>,
    /// Skill folder names whose triggers must not reach the system
    /// prompt: the persisted off switch a frontend's skill management
    /// writes and the agent assembly honors. Discovered-but-disabled
    /// skills may still be listed by the frontend, greyed out; only
    /// the prompt filters them out.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub skills_disabled: BTreeSet<String>,
    /// MCP server names held back from the manager, the prompt, and
    /// the agent's shells: the persisted off switch, the skills set's
    /// twin. Names are matched against the session's sandbox config,
    /// so a name that config does not define is simply inert.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub disabled_mcp_servers: BTreeSet<String>,
    /// Subagent definition folder names held back from discovery: the
    /// persisted off switch, the other two sets' twin. A disabled name
    /// leaves the `subagent` tool's catalog and the prompt; the
    /// built-in included, so a set holding every name drops the tool
    /// whole. Frontends may still list disabled definitions, greyed
    /// out, only discovery filters them.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub disabled_subagents: BTreeSet<String>,
}

/// The profile the flags and stores fall back to: fresh setups seed it,
/// and every `unwrap_or` fallback resolves to it. One name, one owner.
pub const DEFAULT_PROFILE: &str = "personal";

/// The profiles on fresh setups and files that omit the key.
fn default_profiles() -> BTreeMap<String, Profile> {
    BTreeMap::from([
        (DEFAULT_PROFILE.to_owned(), Profile::default()),
        ("work".to_owned(), Profile::default()),
    ])
}

impl Default for Settings {
    /// Fresh setups ship with the two everyday profiles. Loaded
    /// documents missing the key get the same treatment via the
    /// field-level serde default.
    fn default() -> Self {
        Self {
            profiles: default_profiles(),
            skills_disabled: BTreeSet::new(),
            disabled_mcp_servers: BTreeSet::new(),
            disabled_subagents: BTreeSet::new(),
        }
    }
}

impl Settings {
    /// The settings that apply for `flag` (or `personal` when absent).
    ///
    /// ISOLATION: a profile's section is authoritative, work never
    /// inherits personal's model or keys. An unknown name resolves to an
    /// empty profile; the caller decides how to present that (the
    /// frontend says "no provider; log in").
    pub fn effective(&self, flag: Option<&str>) -> Profile {
        self.profiles
            .get(flag.unwrap_or(DEFAULT_PROFILE))
            .cloned()
            .unwrap_or_default()
    }

    /// Mutates the section that writes land in for `flag` (or `personal`
    /// when absent): the named profile, created when new.
    pub fn set_for_profile(&mut self, flag: Option<&str>, apply: impl FnOnce(&mut Profile)) {
        let name = flag
            .map(str::to_owned)
            .unwrap_or_else(|| DEFAULT_PROFILE.to_owned());
        apply(self.profiles.entry(name).or_default());
    }

    /// Moves every stored literal key into the secret store, instance by
    /// instance, best effort: one the store refuses stays literal.
    /// Returns the number moved. Idempotent, an instance already
    /// carrying only its `secret_ref` is skipped, so calling this on
    /// every load costs one refused read per key at most.
    pub fn migrate_secrets(&mut self, secrets: &dyn SecretStore) -> usize {
        let mut moved = 0;
        for profile in self.profiles.values_mut() {
            for instance in profile.instances.values_mut() {
                let Some(key) = instance
                    .api_key
                    .clone()
                    .map(|key| key.trim().to_owned())
                    .filter(|key| !key.is_empty())
                else {
                    continue;
                };
                let id = instance
                    .secret_ref
                    .clone()
                    .unwrap_or_else(crate::secrets::fresh_id);
                if secrets.set(&id, &key).is_ok() {
                    instance.secret_ref = Some(id);
                    instance.api_key = None;
                    moved += 1;
                }
            }
            for instance in profile.websearch.values_mut() {
                moved += migrate_instance_key(instance, secrets);
            }
            if let Some(decision) = profile.decision.as_mut() {
                for instance in decision.instances.values_mut() {
                    moved += migrate_instance_key(instance, secrets);
                }
            }
        }
        moved
    }

    /// Copies every config `secret_ref` the file store does not already
    /// hold from the old read-only OS keyring. Returns how many values
    /// were copied. The config is unchanged, the legacy entries are not
    /// deleted, and a ref already in `secrets` is left alone, so the
    /// call is idempotent and safe on every start.
    pub fn import_legacy_secrets(
        &self,
        legacy: &dyn LegacySecretStore,
        secrets: &dyn SecretStore,
    ) -> usize {
        let mut imported = 0;
        for profile in self.profiles.values() {
            let provider = profile
                .instances
                .values()
                .filter_map(|instance| instance.secret_ref.as_deref());
            let websearch = profile
                .websearch
                .values()
                .filter_map(|instance| instance.secret_ref.as_deref());
            let decision = profile
                .decision
                .iter()
                .flat_map(|decision| decision.instances.values())
                .filter_map(|instance| instance.secret_ref.as_deref());
            for id in provider.chain(websearch).chain(decision) {
                if import_legacy_ref(id, legacy, secrets) {
                    imported += 1;
                }
            }
        }
        imported
    }

    /// Loads the settings from the reference file; a missing file is
    /// the default settings, not an error. A frontend with a file of
    /// its own parses its document and hands the framework its
    /// section, [`Store`] is the seam.
    pub fn load() -> Result<Settings, ConfigError> {
        YamlStore::discover()?.load()
    }

    /// Persists to the reference file, creating the directory `0700`
    /// and the file `0600` on Unix because the section can hold
    /// credentials. Both modes are re-asserted on every save, so a
    /// file that predates the rule, or was widened elsewhere,
    /// tightens again.
    pub fn save(&self) -> Result<(), ConfigError> {
        YamlStore::discover()?.save(self)
    }

    /// [`Settings::load`] against an explicit file (missing file is the
    /// default settings).
    pub fn load_from(path: &impl AsRef<Path>) -> Result<Settings, ConfigError> {
        YamlStore::at(path.as_ref()).load()
    }

    /// [`Settings::save`] to an explicit file, with the same `0600`
    /// handling.
    pub fn save_to(path: &impl AsRef<Path>, settings: &Settings) -> Result<(), ConfigError> {
        YamlStore::at(path.as_ref()).save(settings)
    }
}
/// The config file whole: the framework's section, plus every other
/// consumer's keys. One file carries two vocabularies. The reference
/// terminal stores its look and feel (`icons`, `theme`, and friends)
/// beside the profiles, so a reader of the bare [`Settings`] rejects
/// every file the terminal has written, and saving one back through
/// the bare section would strip it. A consumer that is not the
/// file's owner loads and saves the whole document through this
/// type: bad values inside the settings section still fail the load,
/// and the keys this type does not know land in `rest` and survive
/// the round-trip unchanged.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "snake_case")]
pub struct SettingsDocument {
    /// The framework's section.
    #[serde(flatten)]
    pub settings: Settings,
    /// Every other consumer's keys, carried through untouched.
    #[serde(flatten)]
    pub rest: serde_json::Map<String, serde_json::Value>,
}

impl SettingsDocument {
    /// [`Settings::load`] over the whole file: the discovered
    /// reference file, a missing one included.
    ///
    /// # Errors
    ///
    /// As [`Settings::load`].
    pub fn load() -> Result<Self, ConfigError> {
        Self::load_from(&YamlStore::discover()?.path)
    }

    /// [`Settings::load_from`] over the whole file: a missing file is
    /// the default, and the owner's keys land in `rest`.
    ///
    /// # Errors
    ///
    /// A malformed YAML file, or a bad value inside the settings
    /// section; the owner's keys are never validated here.
    pub fn load_from(path: &impl AsRef<Path>) -> Result<Self, ConfigError> {
        let raw = match std::fs::read_to_string(path.as_ref()) {
            Ok(raw) => raw,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(err) => return Err(err.into()),
        };
        Ok(serde_yaml::from_str(&raw)?)
    }

    /// [`Settings::save_to`] over the whole file: the same `0700`
    /// directory and atomic `0600` write, with the owner's keys
    /// re-emitted after the section.
    ///
    /// # Errors
    ///
    /// As [`Settings::save_to`].
    pub fn save_to(path: &impl AsRef<Path>, document: &Self) -> Result<(), ConfigError> {
        if let Some(parent) = path.as_ref().parent() {
            private_dir(parent)?;
        }
        let yaml = serde_yaml::to_string(document)?;
        private_write_atomic(path.as_ref(), yaml.as_bytes())?;
        Ok(())
    }
}
#[cfg(test)]
mod tests;

/// One legacy copy: only when the destination is reachable and lacks
/// the id, and only when the old keyring has a value. Every failure is
/// a quiet skip; the next start retries the refs still missing.
fn import_legacy_ref(id: &str, legacy: &dyn LegacySecretStore, secrets: &dyn SecretStore) -> bool {
    if !matches!(secrets.get(id), Ok(None)) {
        return false;
    }
    match legacy.get(id) {
        Ok(Some(value)) => secrets.set(id, &value).is_ok(),
        _ => false,
    }
}
