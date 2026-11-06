//! The settings contract between rhema and its frontends.
//!
//! rhema is a framework: the agent driver, the providers, the sandbox
//! and MCP wiring — and any number of frontends built on top, each one
//! owning the user-facing product. This crate is what they agree on:
//! the [`Settings`] every frontend must be able to store and read,
//! whatever file, directory, or format it keeps them in; the [`Store`]
//! seam a frontend implements to put them wherever it likes; the
//! [`Home`] of directory conventions; and the [`secrets`] seam, whose
//! OS keychain backend is namespaced per frontend, so two frontends
//! never share credentials by accident.
//!
//! The reference frontend keeps its whole file — the [`Settings`]
//! section beside its own UI section — at
//! `$XDG_CONFIG_HOME/rhema/config.yaml`, written with `0600`
//! permissions since the section can hold credentials. A different
//! frontend keeps them under its own directory, in its own format: it
//! embeds [`Settings`] as one serde field, implements [`Store`], and
//! the framework itself reads no path and parses no format. See
//! `examples/godcode.rs` for that story in one runnable file.
//!
//! Which profile a session runs under is chosen per invocation
//! (`--profile`) or per session (`/profile`) — it is never stored as a
//! global switch, so parallel instances (work and personal side by
//! side) never fight over one toggle.
//!
//! A profile holds any number of named [`ProviderInstance`]s — two
//! llama.cpp servers, a work and a personal Copilot sub — with one of
//! them active (`provider` + `instance`).
//!
//! A key lives in one of two places: the OS keychain, which the
//! settings reference by an opaque `secret_ref` ([`secrets`]), or the
//! store itself as the literal `api_key` — the fallback on machines
//! with no reachable keychain. [`Profile::place_key`] and
//! [`Settings::migrate_secrets`] prefer the keychain and only fall
//! back to the literal when the store refuses; resolution reads the
//! keychain first on every tier.
//!
//! Environment variables (`RHEMA_PROVIDER`, `GEMINI_API_KEY`,
//! `GOOGLE_API_KEY`, `OPENROUTER_API_KEY`, `ANTHROPIC_API_KEY`,
//! `OPENAI_API_KEY`, `DEEPSEEK_API_KEY`, `ZAI_API_KEY`,
//! `GITHUB_COPILOT_TOKEN`, `RHEMA_SANDBOX`, `RHEMA_SANDBOX_URL`,
//! `RHEMA_SANDBOX_TOKEN`) always win over the stored value — a stored
//! entry is the fallback, not the override.

#![forbid(unsafe_code)]

pub mod mcp;
pub mod privacy;
pub mod secrets;

use crate::privacy::{private_dir, private_write_atomic};
use crate::secrets::SecretStore;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::{Path, PathBuf};

use rhema_core::request::ThinkingEffort;
use serde::{Deserialize, Serialize};

/// Debug helper for credential fields: `Some("[…]")` when a value is
/// present, `None` otherwise — presence without the value, so a
/// `{:?}` of any config shape can never print a live key.
fn redacted(value: &Option<String>) -> Option<&'static str> {
    value.as_ref().map(|_| "[…]")
}

/// Why a config operation failed.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// `HOME` is not set, so the user home — and every convention that
    /// hangs off it, the config base included — cannot be located.
    #[error("cannot locate the home directory: HOME is not set")]
    NoHome,
    /// Reading or writing the file failed.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// The YAML file is not valid or not the expected shape.
    #[error(transparent)]
    Yaml(#[from] serde_yaml::Error),
    /// A foreign [`Store`] refused the section: its own parse or
    /// format error, carried verbatim.
    #[error("settings store: {0}")]
    Store(String),
}

/// One logged-in provider instance: a named credential set for a single
/// provider kind. A profile may hold any number of them — two llama.cpp
/// servers, two OpenAI-compatible endpoints, a work and a personal
/// Copilot sub — keyed by a user-chosen name; the instance's `provider`
/// says which adapter speaks for it.
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "snake_case", deny_unknown_fields)]
pub struct ProviderInstance {
    /// Which adapter serves this instance: `google`, `llama-cpp`,
    /// `openrouter`, `anthropic`, `openai`, `deepseek`, `zai`, or
    /// `copilot`.
    pub provider: String,
    /// The API key (the bearer key for a llama.cpp server), when the
    /// instance needs one and the keychain does not hold it.
    /// Environment variables still win at use time.
    pub api_key: Option<String>,
    /// The keychain entry holding the key, when it is not in this
    /// file: an opaque id under the frontend's keychain service,
    /// resolved through the [`secrets`] seam. Written by
    /// [`Profile::place_key`] and [`Settings::migrate_secrets`]; a
    /// literal `api_key` and a live `secret_ref` never hold the same
    /// key at rest.
    pub secret_ref: Option<String>,
    /// The server base URL: the llama.cpp address, an OpenAI-compatible
    /// endpoint's base, or a Z.ai route override. Unset means the
    /// provider's own default.
    pub base_url: Option<String>,
}

impl fmt::Debug for ProviderInstance {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProviderInstance")
            .field("provider", &self.provider)
            .field("api_key", &redacted(&self.api_key))
            .field("secret_ref", &self.secret_ref)
            .field("base_url", &self.base_url)
            .finish()
    }
}

/// One named place `bash` can run: a user-chosen name over a kind and
/// whatever that kind needs — the sandbox twin of
/// [`ProviderInstance`]. `local` needs no record: the local instance
/// exists by doing nothing.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SandboxInstance {
    /// Which kind runs here: `docker`, `lxd`, `ssh`, `http`, or
    /// `ecrou`. Stored as given — unknown spellings are the
    /// consumer's to resolve or reject.
    pub kind: String,
    /// What the kind names its instance by: the docker container, the
    /// LXD instance, the ssh host, or the http URL. Whitespace-only
    /// values count as unset at use time.
    pub target: Option<String>,
    /// Whether an `lxd` instance is a virtual machine — the launch
    /// flag the assembly stands it up with. Unset is a container, and
    /// every other kind ignores it.
    pub vm: Option<bool>,
}

/// Where `bash` runs when a frontend honors the stored choice: the
/// [`Profile`] section behind the sandbox kinds `local`, `docker`,
/// `ssh`, and `http`. Every field is stored as a string and validated
/// by the consumer — the same stance as every stored name:
/// rhema-config stores, the frontend parses. A profile
/// without this section and a `kind: local` section are the same
/// behavior: today's local process executor.
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "snake_case", deny_unknown_fields)]
pub struct SandboxSettings {
    /// The stored choice: `local`, `docker`, `lxd`, `ssh`, `http`, or
    /// `ecrou`. The name is stored as given — unknown spellings are
    /// the consumer's to resolve or reject.
    pub kind: Option<String>,
    /// The name of the active sandbox instance in
    /// [`Profile::sandbox_instances`] — the one `bash` runs in. Must
    /// name an instance of `kind` to count; `None` is the section's
    /// own single-instance form (the facts below), the shape this map
    /// grew out of.
    pub active: Option<String>,
    /// The `http` sandbox's base URL.
    pub url: Option<String>,
    /// A literal bearer token for the `http` sandbox — the fallback
    /// where no keychain answers, never the first choice.
    pub token: Option<String>,
    /// The keychain entry holding the `http` token, resolved through
    /// the [`secrets`] seam — [`ProviderInstance::secret_ref`]'s
    /// pattern. A literal `token` and a live `token_ref` never hold
    /// the same secret at rest.
    pub token_ref: Option<String>,
    /// The `ssh` destination: `user@host` or an ssh alias, passed to
    /// `ssh` as-is.
    pub host: Option<String>,
    /// The `docker` container to exec in.
    pub container: Option<String>,
    /// The `lxd` instance to exec in — container or virtual machine,
    /// the type riding `vm` (spec §9: one driver and a launch flag,
    /// not two kinds).
    pub instance: Option<String>,
    /// Whether the `lxd` instance is a virtual machine — the launch
    /// flag the assembly stands the instance up with. Unset is a
    /// container.
    pub vm: Option<bool>,
    /// The `ecrou` image every boot resolves: an ecrou distro name
    /// (`ubuntu`, `alpine:3.23`) or an OCI image reference. Unset is
    /// the backend's default, the rhema base
    /// (`ghcr.io/vhaudiquet/rhema-images:latest`); pin a digest for
    /// reproducibility.
    pub image: Option<String>,
    /// Whether the `ecrou` sandbox keeps rootfs state between
    /// commands: installs, daemons, and everything else outside the
    /// workspace survive from one exec to the next, per workspace.
    /// Unset is on; `false` restores the one-command rootfs, where
    /// only the workspace persists.
    pub persist: Option<bool>,
}

impl fmt::Debug for SandboxSettings {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SandboxSettings")
            .field("kind", &self.kind)
            .field("active", &self.active)
            .field("url", &self.url)
            .field("token", &redacted(&self.token))
            .field("token_ref", &self.token_ref)
            .field("host", &self.host)
            .field("container", &self.container)
            .field("instance", &self.instance)
            .field("vm", &self.vm)
            .field("image", &self.image)
            .finish()
    }
}

/// One named set of provider settings. Profiles are isolated: work never
/// inherits personal's model or keys, and vice versa.
///
/// Credentials live in [`Profile::instances`] — named
/// [`ProviderInstance`]s. Unknown keys are rejected: a typo'd `model`
/// must not silently read as unset.
#[derive(Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "snake_case", deny_unknown_fields)]
pub struct Profile {
    /// Which provider serves turns: `google`, `llama-cpp`, `openrouter`,
    /// `anthropic`, `openai`, `deepseek`, `zai`, or `copilot`.
    pub provider: Option<String>,
    /// The name of the active instance in [`Profile::instances`] — the
    /// one whose backend serves turns. Must name an instance of
    /// `provider` to count.
    pub instance: Option<String>,
    /// The logged-in provider instances, keyed by a user-chosen name.
    pub instances: BTreeMap<String, ProviderInstance>,
    /// The last-used model, when one was picked.
    pub model: Option<String>,
    /// The color this profile's name wears in the UI, as `#RRGGBB`.
    /// Unset means the UI picks a tone from the theme by profile name.
    pub color: Option<String>,
    /// How much reasoning the model should do: `low`, `medium`, `high`,
    /// or `default` (the provider decides). Unset, null, and unparseable
    /// all read as `medium` — a level is always named, because
    /// always-thinking models left to their defaults spend the whole
    /// output budget on reasoning and answer with nothing.
    pub thinking: Option<String>,
    /// Where `bash` runs when the frontend honors the choice: the
    /// kind plus whatever that kind needs. `None` and `kind: local`
    /// are the same behavior; the choice machinery engages only
    /// non-local kinds.
    pub sandbox: Option<SandboxSettings>,
    /// The named places `bash` can run, keyed by a user-chosen name —
    /// the sandbox twin of [`Profile::instances`]. Absent from the
    /// file while empty: most profiles never leave `local`.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub sandbox_instances: BTreeMap<String, SandboxInstance>,
    /// The MCP servers this profile exposes to its agents, keyed by a
    /// user-chosen name matching `[a-z0-9_-]+`. Per profile by
    /// design: work and personal carry different servers, and a
    /// server installed through one profile's menu appears nowhere
    /// else. Resolved through [`crate::mcp::McpConfig::from_entries`]
    /// at agent-wiring time — a malformed entry is skipped with a
    /// notice, never a config failure. Absent from the file while
    /// empty; entries' `env` and `headers` values are secrets at rest
    /// and redact in [`Debug`].
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub mcp_servers: BTreeMap<String, crate::mcp::McpServerEntry>,
}

impl fmt::Debug for Profile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Profile")
            .field("provider", &self.provider)
            .field("instance", &self.instance)
            .field("instances", &self.instances)
            .field("model", &self.model)
            .field("color", &self.color)
            .field("thinking", &self.thinking)
            .field("sandbox", &self.sandbox)
            .field("sandbox_instances", &self.sandbox_instances)
            .field("mcp_servers", &self.mcp_servers.keys().collect::<Vec<_>>())
            .finish()
    }
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

    /// The API key `provider` serves: the matching instance's key,
    /// environment tier included; env-only when the profile holds no
    /// instance of it. The provider name is the adapter spelling —
    /// `google`, `llama-cpp`, `openrouter`, `anthropic`, `openai`,
    /// `deepseek`, `zai`, `copilot`.
    pub fn api_key(&self, provider: &str, secrets: Option<&dyn SecretStore>) -> Option<String> {
        match self.instance_for_provider(provider) {
            Some((_, instance)) => self.instance_api_key(instance, secrets),
            None => resolve_key(
                |name| std::env::var(name).ok(),
                provider_env_names(provider),
                None,
            ),
        }
    }

    /// The llama.cpp server URL to use: `LLAMA_CPP_URL`, then the llama
    /// instance's URL, then `http://localhost:8080`.
    pub fn llama_cpp_url(&self) -> String {
        resolve_key(
            |name| std::env::var(name).ok(),
            &["LLAMA_CPP_URL"],
            self.llama_url(),
        )
        .unwrap_or_else(|| "http://localhost:8080".to_owned())
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

    /// The profile's thinking level: the stored name parsed, with unset,
    /// null, and unparseable all reading as [`ThinkingEffort::Medium`].
    /// Only an explicit `default` keeps the provider's own behavior — a
    /// session never silently runs on "send nothing".
    pub fn thinking_effort(&self) -> ThinkingEffort {
        self.thinking
            .as_deref()
            .and_then(ThinkingEffort::parse)
            .unwrap_or(ThinkingEffort::Medium)
    }

    /// Whether this profile can build a backend: it names a provider and
    /// carries the credential that provider needs (a llama.cpp server
    /// works keyless; the rest need a key, env included).
    pub fn has_credentials(&self, secrets: Option<&dyn SecretStore>) -> bool {
        match self.provider.as_deref() {
            Some("llama-cpp") => true,
            Some("openrouter") => self.api_key("openrouter", secrets).is_some(),
            Some("anthropic") => self.api_key("anthropic", secrets).is_some(),
            Some("openai") => self.api_key("openai", secrets).is_some(),
            Some("deepseek") => self.api_key("deepseek", secrets).is_some(),
            Some("zai") => self.api_key("zai", secrets).is_some(),
            Some("copilot") => self.api_key("copilot", secrets).is_some(),
            _ => self.api_key("google", secrets).is_some(),
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
        if self.provider.as_deref() == Some(provider)
            && let Some((name, instance)) = self.active_instance()
        {
            return Some((name, instance));
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
        if let Some(name) = self.instance.as_deref()
            && let Some(instance) = self.instances.get(name)
            && instance.provider == provider
        {
            return Some((name, instance));
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

    /// The sandbox instance a non-local `kind` runs in: the active one
    /// when it is of that kind, else the first stored instance of it,
    /// else `None` — the section's own facts (or the derived name) are
    /// the caller's fallback, the single-instance form this map grew
    /// out of.
    pub fn sandbox_instance_for(&self, kind: &str) -> Option<(&str, &SandboxInstance)> {
        if let Some(name) = self.sandbox.as_ref().and_then(|s| s.active.as_deref())
            && let Some(instance) = self.sandbox_instances.get(name)
            && instance.kind == kind
        {
            return Some((name, instance));
        }
        self.sandbox_instances
            .iter()
            .find(|(_, instance)| instance.kind == kind)
            .map(|(name, instance)| (name.as_str(), instance))
    }

    /// Inserts or replaces the named sandbox instance. The caller
    /// keeps `sandbox.kind`/`sandbox.active` coherent — a new active
    /// instance must be of the stored kind.
    pub fn place_sandbox_instance(&mut self, name: impl Into<String>, instance: SandboxInstance) {
        self.sandbox_instances.insert(name.into(), instance);
    }

    /// Removes the named sandbox instance, clearing the sandbox
    /// section's active pointer when it named the removed one. Returns
    /// the removed record, `None` for a name that was not there.
    pub fn remove_sandbox_instance(&mut self, name: &str) -> Option<SandboxInstance> {
        let removed = self.sandbox_instances.remove(name);
        if removed.is_some()
            && let Some(sandbox) = self.sandbox.as_mut()
            && sandbox.active.as_deref() == Some(name)
        {
            sandbox.active = None;
        }
        removed
    }

    /// The stored key for `provider` without the environment tier: the
    /// keychain entry first, then the literal. Callers that run their
    /// own environment precedence (the claw's discovery) read this
    /// instead of [`Profile::api_key`].
    pub fn stored_provider_key(
        &self,
        provider: &str,
        secrets: Option<&dyn SecretStore>,
    ) -> Option<String> {
        let (_, instance) = self.instance_for_provider(provider)?;
        self.stored_instance_key(instance, secrets)
    }

    /// The stored key of one instance without the environment tier:
    /// the keychain entry `secret_ref` names, then the literal. A
    /// store that cannot be reached reads as "not in the store" — the
    /// next tier serves, never a lost credential.
    pub fn stored_instance_key(
        &self,
        instance: &ProviderInstance,
        secrets: Option<&dyn SecretStore>,
    ) -> Option<String> {
        if let (Some(store), Some(id)) = (secrets, instance.secret_ref.as_deref())
            && let Ok(Some(key)) = store.get(id)
        {
            let trimmed = key.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_owned());
            }
        }
        instance
            .api_key
            .clone()
            .map(|key| key.trim().to_owned())
            .filter(|key| !key.is_empty())
    }

    /// The key an instance serves: the provider's environment variables
    /// first, then the stored key — keychain, then literal.
    /// Whitespace-only values count as unset at every tier.
    pub fn instance_api_key(
        &self,
        instance: &ProviderInstance,
        secrets: Option<&dyn SecretStore>,
    ) -> Option<String> {
        resolve_key(
            |name| std::env::var(name).ok(),
            provider_env_names(&instance.provider),
            self.stored_instance_key(instance, secrets),
        )
    }

    /// Places a just-entered key on the named instance: the keychain,
    /// when the store accepts it — the instance then carries only its
    /// `secret_ref` — else the literal, the shape machines without a
    /// store have always held. `None` when no instance bears `name`.
    pub fn place_key(
        &mut self,
        name: &str,
        key: &str,
        secrets: Option<&dyn SecretStore>,
    ) -> Option<KeyLocation> {
        let instance = self.instances.get_mut(name)?;
        let trimmed = key.trim();
        if trimmed.is_empty() || secrets.is_none() {
            instance.api_key = Some(trimmed.to_owned());
            return Some(KeyLocation::File);
        }
        let store = secrets.expect("checked above");
        let id = instance
            .secret_ref
            .clone()
            .unwrap_or_else(crate::secrets::fresh_id);
        match store.set(&id, trimmed) {
            Ok(()) => {
                instance.secret_ref = Some(id);
                instance.api_key = None;
                Some(KeyLocation::Keychain)
            }
            // The keychain refused: the file keeps the key, the
            // behavior it has always had. Nothing is lost.
            Err(_) => {
                instance.api_key = Some(trimmed.to_owned());
                Some(KeyLocation::File)
            }
        }
    }

    /// The base URL an instance serves: `LLAMA_CPP_URL` over the stored
    /// URL for llama.cpp; the stored base for OpenAI-compatible and Z.ai
    /// instances (the caller defaults each kind, so its own default
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
            "openai" | "zai" => instance
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

    /// The llama instance's URL, without the default fallback.
    fn llama_url(&self) -> Option<String> {
        self.instance_url_for("llama-cpp")
    }

    /// The sandbox kind this profile runs `bash` on: `RHEMA_SANDBOX`,
    /// then the stored `kind`. Whitespace-only values count as unset.
    /// The name is returned as given — `local`, `docker`, `ssh`,
    /// `http`, and `ecrou` are the spellings the sandbox design names;
    /// unknown ones are the consumer's to resolve or reject.
    pub fn sandbox_kind(&self) -> Option<String> {
        resolve_key(
            |name| std::env::var(name).ok(),
            &["RHEMA_SANDBOX"],
            self.sandbox.as_ref().and_then(|s| s.kind.clone()),
        )
    }

    /// The `http` sandbox's base URL: `RHEMA_SANDBOX_URL`, then the
    /// stored `url`. Whitespace-only values count as unset.
    pub fn sandbox_url(&self) -> Option<String> {
        resolve_key(
            |name| std::env::var(name).ok(),
            &["RHEMA_SANDBOX_URL"],
            self.sandbox.as_ref().and_then(|s| s.url.clone()),
        )
    }

    /// The bearer token the `http` sandbox authenticates with:
    /// `RHEMA_SANDBOX_TOKEN`, then the keychain entry `token_ref`
    /// names, then the literal `token`. Whitespace-only values count
    /// as unset at every tier.
    pub fn sandbox_token(&self, secrets: Option<&dyn SecretStore>) -> Option<String> {
        resolve_key(
            |name| std::env::var(name).ok(),
            &["RHEMA_SANDBOX_TOKEN"],
            self.stored_sandbox_token(secrets),
        )
    }

    /// The stored sandbox token without the environment tier: the
    /// keychain entry `token_ref` names first, then the literal
    /// `token`. A store that cannot be reached reads as "not in the
    /// store" — the next tier serves, never a lost token. Callers that
    /// run their own environment precedence read this instead of
    /// [`Profile::sandbox_token`].
    pub fn stored_sandbox_token(&self, secrets: Option<&dyn SecretStore>) -> Option<String> {
        let sandbox = self.sandbox.as_ref()?;
        if let (Some(store), Some(id)) = (secrets, sandbox.token_ref.as_deref())
            && let Ok(Some(token)) = store.get(id)
        {
            let trimmed = token.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_owned());
            }
        }
        sandbox
            .token
            .clone()
            .map(|token| token.trim().to_owned())
            .filter(|token| !token.is_empty())
    }

    /// The `ssh` destination as stored — `user@host` or an ssh alias,
    /// passed to `ssh` as-is. Stored only, no environment tier: a
    /// stray variable must not silently move where commands run.
    /// Whitespace-only values count as unset.
    pub fn sandbox_host(&self) -> Option<String> {
        self.sandbox
            .as_ref()
            .and_then(|s| s.host.clone())
            .map(|host| host.trim().to_owned())
            .filter(|host| !host.is_empty())
    }

    /// The `docker` container to exec in, as stored. Stored only — no
    /// environment tier, the same stance as [`Profile::sandbox_host`].
    /// Whitespace-only values count as unset.
    pub fn sandbox_container(&self) -> Option<String> {
        self.sandbox
            .as_ref()
            .and_then(|s| s.container.clone())
            .map(|c| c.trim().to_owned())
            .filter(|c| !c.is_empty())
    }

    /// The `lxd` instance to exec in, as stored. Stored only — the
    /// same stance as [`Profile::sandbox_container`]. Whitespace-only
    /// values count as unset.
    pub fn sandbox_instance(&self) -> Option<String> {
        self.sandbox
            .as_ref()
            .and_then(|s| s.instance.clone())
            .map(|i| i.trim().to_owned())
            .filter(|i| !i.is_empty())
    }

    /// Whether the `lxd` instance is a virtual machine. Stored only;
    /// unset is a container.
    pub fn sandbox_vm(&self) -> bool {
        self.sandbox.as_ref().and_then(|s| s.vm).unwrap_or(false)
    }

    /// The `ecrou` image every boot resolves, as stored. Stored only —
    /// the same stance as [`Profile::sandbox_container`].
    /// Whitespace-only values count as unset.
    pub fn sandbox_image(&self) -> Option<String> {
        self.sandbox
            .as_ref()
            .and_then(|s| s.image.clone())
            .map(|image| image.trim().to_owned())
            .filter(|image| !image.is_empty())
    }

    /// Whether the `ecrou` sandbox keeps rootfs state between
    /// commands. Default on: a session's installs survive its
    /// commands, which is what an agent session expects of a machine.
    /// `sandbox.persist: false` restores the one-command rootfs.
    pub fn sandbox_persist(&self) -> bool {
        self.sandbox
            .as_ref()
            .and_then(|s| s.persist)
            .unwrap_or(true)
    }
}

/// Where [`Profile::place_key`] landed a credential.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyLocation {
    /// The OS keychain: the instance carries only the entry's
    /// `secret_ref`.
    Keychain,
    /// The config file itself: no keychain answered, so the literal
    /// stayed — the shape the file has always held.
    File,
}

/// The environment variables whose values override a stored key for
/// `provider`, in precedence order.
/// One provider's config-facing facts, one row per adapter: the
/// spelling the config file and `RHEMA_PROVIDER` use, the environment
/// variables a credential resolves through, and the fix to name when
/// none does. Adding a provider means adding a row here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProviderInfo {
    /// The config spelling: `google`, `llama-cpp`, `openrouter`,
    /// `anthropic`, `openai`, `deepseek`, `zai`, `copilot`.
    pub name: &'static str,
    /// The environment variables a credential resolves through, in
    /// priority order.
    pub env_keys: &'static [&'static str],
    /// Whether the provider has a login flow: a credential the user
    /// enters or a device grant answers for. `false` for llama.cpp —
    /// a server is an address to point at, not a subscription.
    pub login: bool,
    /// The fix a frontend without a login screen names when no
    /// credential resolves for this provider.
    pub missing_key_help: &'static str,
}

/// The provider registry, in the order the pickers list them.
pub const PROVIDERS: &[ProviderInfo] = &[
    ProviderInfo {
        name: "google",
        env_keys: &["GEMINI_API_KEY", "GOOGLE_API_KEY"],
        login: true,
        missing_key_help: "missing API key: set GEMINI_API_KEY (or log in once interactively)",
    },
    ProviderInfo {
        name: "llama-cpp",
        env_keys: &["LLAMA_CPP_API_KEY"],
        login: false,
        missing_key_help: "missing API key: set LLAMA_CPP_API_KEY",
    },
    ProviderInfo {
        name: "openrouter",
        env_keys: &["OPENROUTER_API_KEY"],
        login: true,
        missing_key_help: "missing API key: set OPENROUTER_API_KEY",
    },
    ProviderInfo {
        name: "anthropic",
        env_keys: &["ANTHROPIC_API_KEY"],
        login: true,
        missing_key_help: "missing API key: set ANTHROPIC_API_KEY (or log in once interactively)",
    },
    ProviderInfo {
        name: "openai",
        env_keys: &["OPENAI_API_KEY"],
        login: true,
        missing_key_help: "missing API key: set OPENAI_API_KEY (or log in once interactively)",
    },
    ProviderInfo {
        name: "deepseek",
        env_keys: &["DEEPSEEK_API_KEY"],
        login: true,
        missing_key_help: "missing API key: set DEEPSEEK_API_KEY (or log in once interactively)",
    },
    ProviderInfo {
        name: "zai",
        env_keys: &["ZAI_API_KEY", "Z_AI_API_KEY"],
        login: true,
        missing_key_help: "missing API key: set ZAI_API_KEY (or log in once interactively)",
    },
    ProviderInfo {
        name: "copilot",
        env_keys: &["GITHUB_COPILOT_TOKEN"],
        login: true,
        missing_key_help: "missing API key: set GITHUB_COPILOT_TOKEN (or log in once interactively)",
    },
];

/// Looks a provider up by config spelling; `None` for anything no row
/// answers to.
pub fn provider_info(name: &str) -> Option<&'static ProviderInfo> {
    PROVIDERS.iter().find(|info| info.name == name)
}

fn provider_env_names(provider: &str) -> &'static [&'static str] {
    provider_info(provider)
        .map(|info| info.env_keys)
        .unwrap_or(&[])
}

/// The framework's own section of the settings: what any frontend
/// built on rhema must be able to store and read, whatever document
/// it co-locates them in. Everything beyond this — themes, palettes,
/// onboarding state, per-workspace reopen memory — is the frontend's
/// own section, in the frontend's own file.
///
/// Unknown keys are rejected: a typo'd key must not silently read as
/// its default. A frontend embedding the section into a bigger
/// document inherits the same strictness for these keys — a typo in
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
    /// twin. Names are matched against every scope's servers (the
    /// global file and each profile), so a name another profile does
    /// not define is simply inert.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub disabled_mcp_servers: BTreeSet<String>,
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
        }
    }
}

impl Settings {
    /// The settings that apply for `flag` (or `personal` when absent).
    ///
    /// ISOLATION: a profile's section is authoritative — work never
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

    /// Moves every stored literal key into the keychain, instance by
    /// instance, best effort: one the store refuses stays literal.
    /// Returns the number moved. Idempotent — an instance already
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
        }
        moved
    }

    /// Loads the settings from the reference file; a missing file is
    /// the default settings, not an error. A frontend with a file of
    /// its own parses its document and hands the framework its
    /// section — [`Store`] is the seam.
    pub fn load() -> Result<Settings, ConfigError> {
        YamlStore::discover()?.load()
    }

    /// Persists to the reference file, creating the directory `0700`
    /// and the file `0600` on Unix because the section can hold
    /// credentials. Both modes are re-asserted on every save, so a
    /// file that predates the rule — or was widened elsewhere —
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

/// The directory conventions a session reads: where the settings
/// file, the user-wide skills, the subagent definitions, and the base
/// system prompt live — plus the user home, because the `.agents`
/// skills-sharing convention lives there and nowhere else.
///
/// The reference layout resolves both roots from the environment
/// ([`Home::discover`]). A frontend that owns its own directory keeps
/// its settings — and everything derived from them — under that
/// directory instead ([`Home::relocated`]): `~/.godcode/config.yaml`,
/// `~/.godcode/skills`, while the cross-agent convention stays at
/// `~/.agents/skills`, where every tool shares it. Nothing downstream
/// reaches for the environment again; it asks the [`Home`] it was
/// handed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Home {
    /// The user's home directory: where the `.agents/skills` sharing
    /// convention lives — every tool's skills, not any frontend's.
    pub home: PathBuf,
    /// The config base: `<config-base>/rhema` for the reference
    /// layout, the frontend's own directory for a relocated one.
    pub config: PathBuf,
}

impl Home {
    /// The reference layout: `config` under `XDG_CONFIG_HOME` (or
    /// `~/.config`), `home` under `HOME`. The shape a machine running
    /// the rhema frontend presents.
    pub fn discover() -> Result<Self, ConfigError> {
        Ok(Self {
            home: user_home()?,
            config: reference_config_base()?,
        })
    }

    /// A relocated config base under the discovered home: the seam
    /// for a frontend whose settings live in their own directory.
    /// Skills, subagents, and the base prompt follow the config base;
    /// the `.agents` convention stays machine-global.
    pub fn relocated(config: PathBuf) -> Result<Self, ConfigError> {
        Ok(Self {
            home: user_home()?,
            config,
        })
    }

    /// Both roots explicit — containers and tests, where no
    /// environment variable may answer.
    pub fn at(home: PathBuf, config: PathBuf) -> Self {
        Self { home, config }
    }

    /// The reference layout under an explicit home — the seam for a
    /// test isolating itself from the machine: `config` under
    /// `<home>/.config/rhema`, the same shape [`Self::discover`]
    /// resolves, no environment variable consulted.
    pub fn under_home(home: &Path) -> Self {
        Self {
            home: home.to_path_buf(),
            config: home.join(".config").join("rhema"),
        }
    }

    /// The settings file (`<config>/config.yaml`) under the reference
    /// YAML store; another format keeps its own name beside it.
    pub fn config_file(&self) -> PathBuf {
        self.config.join("config.yaml")
    }

    /// The user-wide skills directory (`<config>/skills`); a workspace
    /// can add more under its own root.
    pub fn skills(&self) -> PathBuf {
        self.config.join("skills")
    }

    /// The cross-agent skills directory (`<home>/.agents/skills`),
    /// where the skill-sharing convention keeps user-wide skills.
    /// Discovery reads it at the bottom of the shadowing order, so a
    /// rhema skill of the same name always wins.
    pub fn agents_skills(&self) -> PathBuf {
        self.home.join(".agents").join("skills")
    }

    /// The user-wide subagent definitions directory
    /// (`<config>/subagents`); a workspace can add more under its own
    /// `.rhema/subagents`.
    pub fn subagents(&self) -> PathBuf {
        self.config.join("subagents")
    }

    /// The editable base system prompt (`<config>/SYSTEM.md`). A
    /// `system edit` command seeds it with the built-in default,
    /// a `system restore` restores it; nothing is ever written at
    /// runtime — the file is an opt-in override.
    pub fn system_prompt(&self) -> PathBuf {
        self.config.join("SYSTEM.md")
    }
}

/// The user's home directory (`$HOME`).
fn user_home() -> Result<PathBuf, ConfigError> {
    match std::env::var("HOME") {
        Ok(home) if !home.trim().is_empty() => Ok(PathBuf::from(home)),
        _ => Err(ConfigError::NoHome),
    }
}

/// The reference config base (`$XDG_CONFIG_HOME/rhema`, or
/// `~/.config/rhema`).
fn reference_config_base() -> Result<PathBuf, ConfigError> {
    match std::env::var("XDG_CONFIG_HOME") {
        Ok(dir) if !dir.trim().is_empty() => Ok(PathBuf::from(dir).join("rhema")),
        _ => Ok(user_home()?.join(".config").join("rhema")),
    }
}

/// The settings-storage seam: where the [`Settings`] section comes
/// from and goes. The framework ships the YAML file store; a frontend
/// that owns its own document — its own directory, its own format —
/// implements this over its embed of the section, and nothing
/// downstream knows a file or a format exists.
pub trait Store: Send + Sync {
    /// The stored settings; a missing store is the default settings,
    /// not an error.
    fn load(&self) -> Result<Settings, ConfigError>;
    /// Persists the settings. A frontend co-locating the section with
    /// its own keys re-serializes its whole document around it, so the
    /// document's other sections survive — best effort, as file
    /// semantics allow.
    fn save(&self, settings: &Settings) -> Result<(), ConfigError>;
}

/// The reference [`Store`]: `config.yaml` in YAML, the shape the file
/// has always had. Atomic and `0600` — the section can hold literal
/// credentials — with the directory created `0700`; both modes are
/// re-asserted on every save, so a file that predates the rule — or
/// was widened elsewhere — tightens again.
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

/// The platform a frontend hands the framework: the directory
/// conventions, the settings store, and the keychain namespace — the
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
    fn debug_prints_presence_never_the_key() {
        let profile = Profile {
            provider: Some("google".into()),
            instances: BTreeMap::from([(
                "main".into(),
                ProviderInstance {
                    provider: "google".into(),
                    api_key: Some("sk-live-secret".into()),
                    secret_ref: None,
                    base_url: None,
                },
            )]),
            sandbox: Some(SandboxSettings {
                kind: Some("http".into()),
                token: Some("bearer-secret".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let printed = format!("{profile:?}");
        assert!(!printed.contains("sk-live-secret"), "{printed}");
        assert!(!printed.contains("bearer-secret"), "{printed}");
        assert!(printed.contains("Some(\"[…]\")"), "{printed}");
    }

    #[test]
    fn disabled_skills_round_trip_and_default_to_quiet() {
        // No skill is off by default.
        assert!(Settings::default().skills_disabled.is_empty());

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.yaml");
        let settings = Settings {
            skills_disabled: BTreeSet::from(["commit".to_owned(), "review".to_owned()]),
            ..Settings::default()
        };
        Settings::save_to(&path, &settings).unwrap();
        let loaded = Settings::load_from(&path).unwrap();
        assert_eq!(
            loaded.skills_disabled,
            BTreeSet::from(["commit".to_owned(), "review".to_owned()])
        );
        // A file written before the field existed keeps reading as the
        // quiet state.
        std::fs::write(&path, "profiles: {}\n").unwrap();
        let old = Settings::load_from(&path).unwrap();
        assert!(old.skills_disabled.is_empty());
    }

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
        let keyed = |provider: &str| {
            let mut profile = Profile {
                provider: Some(provider.to_owned()),
                ..Profile::default()
            };
            profile.upsert_instance(
                provider,
                ProviderInstance {
                    provider: provider.to_owned(),
                    api_key: Some("k".to_owned()),
                    secret_ref: None,
                    base_url: None,
                },
            );
            profile
        };
        // A stored key makes the profile usable...
        assert!(keyed("anthropic").has_credentials(None));
        assert!(keyed("openai").has_credentials(None));
        assert!(keyed("deepseek").has_credentials(None));
        assert!(keyed("copilot").has_credentials(None));
        // ...and each provider needs exactly its own key.
        assert!(
            keyed("anthropic")
                .stored_provider_key("openai", None)
                .is_none()
        );
        assert!(
            keyed("openai")
                .stored_provider_key("anthropic", None)
                .is_none()
        );
        // No stored key and no env var: not usable. (Machines running the
        // suite with these env vars exported would see the env key win,
        // per the convention above.)
        let bare = |provider: &str| Profile {
            provider: Some(provider.to_owned()),
            ..Profile::default()
        };
        assert!(!bare("anthropic").has_credentials(None));
        assert!(!bare("openai").has_credentials(None));
        assert!(!bare("deepseek").has_credentials(None));
        assert!(!bare("copilot").has_credentials(None));
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
    fn profile_mcp_servers_round_trip_and_resolve_and_redact() {
        let yaml = r#"
profiles:
  personal:
    provider: llama-cpp
    mcp_servers:
      atlassian:
        description: Jira and Confluence
        command: ["npx", "-y", "@modelcontextprotocol/server-atlassian"]
        env: { JIRA_TOKEN: "hunter2" }
      blender:
        description: Blender scene operations
        url: "http://127.0.0.1:9721/mcp"
        headers: { Authorization: "Bearer hunter2" }
      broken:
        description: both transports named
        command: ["true"]
        url: "http://127.0.0.1:1/mcp"
"#;
        let settings: Settings = serde_yaml::from_str(yaml).expect("settings parse");
        let profile = &settings.profiles["personal"];
        assert_eq!(profile.mcp_servers.len(), 3);

        // The round trip: serialize back, empty map absent, entry
        // fields stable.
        let round = serde_yaml::to_string(&settings).unwrap();
        assert!(!round.contains("mcp_servers: {}"));
        assert!(round.contains("atlassian:"));

        // Resolve: two valid servers, the transport-XOR entry skips
        // with a notice naming it.
        let resolved = crate::mcp::McpConfig::from_entries(&profile.mcp_servers);
        assert_eq!(resolved.servers.len(), 2);
        assert!(resolved.notices.iter().any(|n| n.contains("`broken`")));

        // Secrets redact in the entry's Debug: values gone, keys
        // present. The profile's own Debug names servers only.
        let debug = format!("{:?}", profile.mcp_servers["atlassian"]);
        assert!(!debug.contains("hunter2"));
        assert!(debug.contains("JIRA_TOKEN"));
        assert!(!format!("{profile:?}").contains("hunter2"));
    }

    #[test]
    fn config_round_trips_through_disk_as_yaml() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.yaml");
        let config = Settings {
            profiles: BTreeMap::from([(
                "personal".to_owned(),
                Profile {
                    sandbox_instances: BTreeMap::new(),
                    mcp_servers: BTreeMap::new(),
                    provider: Some("llama-cpp".into()),
                    instance: Some("home server".into()),
                    instances: BTreeMap::from([
                        (
                            "home server".to_owned(),
                            ProviderInstance {
                                provider: "llama-cpp".into(),
                                api_key: None,
                                secret_ref: None,
                                base_url: Some("http://localhost:8080".into()),
                            },
                        ),
                        (
                            "anthropic".to_owned(),
                            ProviderInstance {
                                provider: "anthropic".into(),
                                api_key: Some("sk-ant-123".into()),
                                secret_ref: None,
                                base_url: None,
                            },
                        ),
                        (
                            "copilot".to_owned(),
                            ProviderInstance {
                                provider: "copilot".into(),
                                api_key: Some("gho-123".into()),
                                secret_ref: None,
                                base_url: None,
                            },
                        ),
                        (
                            "deepseek".to_owned(),
                            ProviderInstance {
                                provider: "deepseek".into(),
                                api_key: Some("sk-dsk-123".into()),
                                secret_ref: None,
                                base_url: None,
                            },
                        ),
                    ]),
                    model: Some("gemini-3.8-flash".into()),
                    color: None,
                    thinking: Some("low".into()),
                    sandbox: None,
                },
            )]),
            skills_disabled: BTreeSet::new(),
            disabled_mcp_servers: BTreeSet::new(),
        };
        Settings::save_to(&path, &config).unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.contains("instances:"), "yaml on disk: {raw}");
        assert!(raw.contains("sk-ant-123"), "yaml on disk: {raw}");
        assert!(raw.contains("gho-123"), "yaml on disk: {raw}");
        assert!(raw.contains("sk-dsk-123"), "yaml on disk: {raw}");
        assert!(!raw.contains('{'), "not json: {raw}");
        let loaded = Settings::load_from(&path).unwrap();
        assert_eq!(loaded, config);

        // The thinking level parses; unset and junk read as the medium
        // default, an explicit `default` keeps the provider's own.
        let mut config = config;
        assert_eq!(
            config.profiles["personal"].thinking_effort(),
            ThinkingEffort::Low
        );
        config.profiles.get_mut("personal").unwrap().thinking = Some("maximum".into());
        assert_eq!(
            config.profiles["personal"].thinking_effort(),
            ThinkingEffort::Medium
        );
        config.profiles.get_mut("personal").unwrap().thinking = None;
        assert_eq!(
            config.profiles["personal"].thinking_effort(),
            ThinkingEffort::Medium
        );
        config.profiles.get_mut("personal").unwrap().thinking = Some("default".into());
        assert_eq!(
            config.profiles["personal"].thinking_effort(),
            ThinkingEffort::Default
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "credentials file must be 0600");
        }
    }

    #[test]
    fn the_sandbox_instance_resolves_through_the_active_name_then_the_first() {
        let mut profile = Profile {
            sandbox: Some(SandboxSettings {
                kind: Some("lxd".into()),
                active: Some("dev-vm".into()),
                ..SandboxSettings::default()
            }),
            ..Profile::default()
        };
        profile.place_sandbox_instance(
            "dev-vm",
            SandboxInstance {
                kind: "lxd".into(),
                target: Some("dev".into()),
                vm: Some(true),
            },
        );
        profile.place_sandbox_instance(
            "build",
            SandboxInstance {
                kind: "lxd".into(),
                target: Some("build".into()),
                vm: None,
            },
        );
        // The active pointer names the one.
        let (name, instance) = profile.sandbox_instance_for("lxd").unwrap();
        assert_eq!(name, "dev-vm");
        assert_eq!(instance.target.as_deref(), Some("dev"));
        // A pointer at a different kind falls through to the first of
        // the kind asked for.
        profile.sandbox.as_mut().unwrap().active = Some("build".into());
        let (name, instance) = profile.sandbox_instance_for("lxd").unwrap();
        assert_eq!(name, "build", "the pointer names an lxd instance");
        assert_eq!(instance.target.as_deref(), Some("build"));
        profile.sandbox.as_mut().unwrap().active = Some("gone".into());
        let (name, _) = profile.sandbox_instance_for("lxd").unwrap();
        assert_eq!(name, "build", "a dangling pointer falls through");
        // No instance of the kind: the section's facts are the
        // caller's fallback.
        assert_eq!(profile.sandbox_instance_for("docker"), None);
    }

    #[test]
    fn removing_a_sandbox_instance_clears_the_active_pointer() {
        let mut profile = Profile {
            sandbox: Some(SandboxSettings {
                kind: Some("lxd".into()),
                active: Some("dev-vm".into()),
                ..SandboxSettings::default()
            }),
            ..Profile::default()
        };
        profile.place_sandbox_instance(
            "dev-vm",
            SandboxInstance {
                kind: "lxd".into(),
                target: Some("dev".into()),
                vm: None,
            },
        );
        profile.place_sandbox_instance(
            "build",
            SandboxInstance {
                kind: "lxd".into(),
                target: Some("build".into()),
                vm: None,
            },
        );
        let removed = profile.remove_sandbox_instance("dev-vm").unwrap();
        assert_eq!(removed.target.as_deref(), Some("dev"));
        assert_eq!(
            profile.sandbox.as_ref().unwrap().active,
            None,
            "the pointer went with it"
        );
        assert_eq!(profile.sandbox_instance_for("lxd").unwrap().0, "build");
        // Removing a name that is not there removes nothing.
        assert!(profile.remove_sandbox_instance("dev-vm").is_none());
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
                        secret_ref: None,
                        base_url: Some("http://gpu:8080".into()),
                    },
                ),
                (
                    "home".to_owned(),
                    ProviderInstance {
                        provider: "llama-cpp".into(),
                        api_key: None,
                        secret_ref: None,
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
                secret_ref: None,
                base_url: None,
            },
        );
        profile.upsert_instance(
            "proxy",
            ProviderInstance {
                provider: "openai".into(),
                api_key: Some("sk-proxy".into()),
                secret_ref: None,
                base_url: Some("http://localhost:1234/v1".into()),
            },
        );
        assert_eq!(
            profile.api_key("openai", None).as_deref(),
            Some("sk-personal")
        );
        assert_eq!(
            profile.openai_base_url().as_deref(),
            Some("https://api.openai.com/v1")
        );
        // Activating the proxy instance moves both the key and the base.
        profile.instance = Some("proxy".into());
        assert_eq!(profile.api_key("openai", None).as_deref(), Some("sk-proxy"));
        assert_eq!(
            profile.openai_base_url().as_deref(),
            Some("http://localhost:1234/v1")
        );
        assert!(profile.has_credentials(None));
        // Pointing at a missing name falls back to the first openai
        // instance rather than failing open.
        profile.instance = Some("nope".into());
        assert_eq!(
            profile.api_key("openai", None).as_deref(),
            Some("sk-personal")
        );

        // The env-free read serves callers with their own precedence.
        profile.instance = Some("proxy".into());
        assert_eq!(
            profile.stored_provider_key("openai", None).as_deref(),
            Some("sk-proxy")
        );
    }

    #[test]
    fn zai_keys_read_the_two_env_names_with_zai_api_key_first() {
        let env = |name: &str| match name {
            "ZAI_API_KEY" => Some("zai-from-env".into()),
            "Z_AI_API_KEY" => Some("z-ai-from-env".into()),
            _ => None,
        };
        assert_eq!(
            resolve_key(env, provider_env_names("zai"), Some("stored".into())),
            Some("zai-from-env".into())
        );
        // The alias serves when the primary name is unset.
        let env = |name: &str| match name {
            "Z_AI_API_KEY" => Some("z-ai-from-env".into()),
            _ => None,
        };
        assert_eq!(
            resolve_key(env, provider_env_names("zai"), None),
            Some("z-ai-from-env".into())
        );
    }

    #[test]
    fn a_zai_instance_honors_its_stored_base_url() {
        let mut profile = Profile {
            provider: Some("zai".into()),
            instance: Some("plan".into()),
            ..Profile::default()
        };
        profile.upsert_instance(
            "plan",
            ProviderInstance {
                provider: "zai".into(),
                api_key: Some("zkey".into()),
                secret_ref: None,
                base_url: Some("https://proxy.example/api/anthropic".into()),
            },
        );
        assert_eq!(profile.api_key("zai", None).as_deref(), Some("zkey"));
        assert_eq!(
            profile.instance_url_for("zai").as_deref(),
            Some("https://proxy.example/api/anthropic")
        );
        assert!(profile.has_credentials(None));
        // A blank stored base reads as unset: the caller defaults the
        // route, the same convention the openai kind follows.
        profile.upsert_instance(
            "plan",
            ProviderInstance {
                provider: "zai".into(),
                api_key: Some("zkey".into()),
                secret_ref: None,
                base_url: Some("   ".into()),
            },
        );
        assert_eq!(profile.instance_url_for("zai"), None);
    }

    #[test]
    #[cfg(unix)]
    fn a_widened_config_file_tightens_on_the_next_save() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.yaml");
        std::fs::write(&path, "stale").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        Settings::save_to(&path, &Settings::default()).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "credentials file must be 0600");
    }

    #[test]
    fn a_missing_file_loads_as_the_default_config() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("does-not-exist.yaml");
        assert_eq!(Settings::load_from(&path).unwrap(), Settings::default());
    }

    #[test]
    fn fresh_setups_ship_personal_and_work_profiles() {
        let config = Settings::default();
        assert!(config.profiles.contains_key("personal"));
        assert!(config.profiles.contains_key("work"));
        // A loaded file missing the key gets the same seeding.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.yaml");
        std::fs::write(&path, "skills_disabled: [commit]\n").unwrap();
        let loaded = Settings::load_from(&path).unwrap();
        assert!(loaded.profiles.contains_key("work"));
        assert_eq!(
            loaded.skills_disabled,
            BTreeSet::from(["commit".to_owned()])
        );
    }

    #[test]
    fn an_unknown_key_is_rejected_and_named() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.yaml");
        std::fs::write(&path, "modle: claude-sonnet-4-5\n").unwrap();
        let err = Settings::load_from(&path).expect_err("a typo'd key must not read as a default");
        let message = err.to_string();
        assert!(
            message.contains("modle") && message.contains("expected one of"),
            "the error names the key: {message}"
        );
        // The same strictness inside a profile.
        std::fs::write(&path, "profiles:\n  personal:\n    provder: anthropic\n").unwrap();
        let err = Settings::load_from(&path).expect_err("a typo'd profile key is loud too");
        assert!(err.to_string().contains("provder"), "got: {err}");
    }

    #[test]
    fn profiles_are_isolated_from_each_other() {
        let keyed = |provider: &str, key: &str| {
            let mut profile = Profile {
                provider: Some(provider.to_owned()),
                ..Profile::default()
            };
            profile.upsert_instance(
                provider,
                ProviderInstance {
                    provider: provider.to_owned(),
                    api_key: Some(key.to_owned()),
                    secret_ref: None,
                    base_url: None,
                },
            );
            profile
        };
        let config = Settings {
            profiles: BTreeMap::from([
                (
                    "personal".to_owned(),
                    Profile {
                        model: Some("personal-model".into()),
                        ..keyed("google", "personal-key")
                    },
                ),
                (
                    "work".to_owned(),
                    Profile {
                        color: Some("#8FA6B8".into()),
                        ..keyed("openrouter", "work-key")
                    },
                ),
            ]),
            skills_disabled: BTreeSet::new(),
            disabled_mcp_servers: BTreeSet::new(),
        };
        let work = config.effective(Some("work"));
        assert_eq!(work.provider.as_deref(), Some("openrouter"));
        assert_eq!(work.stored_provider_key("google", None).as_deref(), None);
        assert_eq!(
            work.stored_provider_key("openrouter", None).as_deref(),
            Some("work-key")
        );
        assert_eq!(work.model, None);
        assert_eq!(work.color.as_deref(), Some("#8FA6B8"));

        // personal's fields never leak into work, and vice versa.
        let personal = config.effective(Some("personal"));
        assert_eq!(
            personal.stored_provider_key("google", None).as_deref(),
            Some("personal-key")
        );
        assert_eq!(personal.model.as_deref(), Some("personal-model"));

        // An unknown name resolves to an empty profile: nothing leaks in.
        assert_eq!(config.effective(Some("other")), Profile::default());
    }

    #[test]
    fn set_for_profile_writes_the_named_section() {
        let mut config = Settings::default();
        config.set_for_profile(Some("work"), |p| {
            p.upsert_instance(
                "google",
                ProviderInstance {
                    provider: "google".into(),
                    api_key: Some("work-key".into()),
                    secret_ref: None,
                    base_url: None,
                },
            );
        });
        // Unflagged writes land on personal, the default profile.
        config.set_for_profile(None, |p| p.model = Some("m".into()));
        assert_eq!(
            config.profiles["work"]
                .stored_provider_key("google", None)
                .as_deref(),
            Some("work-key")
        );
        assert_eq!(config.profiles["personal"].model.as_deref(), Some("m"));
        // work's section is untouched by personal's writes.
        assert_eq!(config.profiles["work"].model, None);
    }

    /// A store that answers nothing: the shape a machine without a
    /// keychain presents.
    struct UnreachableStore;

    impl crate::secrets::SecretStore for UnreachableStore {
        fn get(&self, _: &str) -> Result<Option<String>, crate::secrets::SecretError> {
            Err(crate::secrets::SecretError::NoStore)
        }
        fn set(&self, _: &str, _: &str) -> Result<(), crate::secrets::SecretError> {
            Err(crate::secrets::SecretError::NoStore)
        }
        fn delete(&self, _: &str) -> Result<bool, crate::secrets::SecretError> {
            Err(crate::secrets::SecretError::NoStore)
        }
    }

    /// The keychain tier serves before the literal, an unreachable
    /// store falls through to it, and the environment tier is covered
    /// by the pure resolver tests above.
    #[test]
    fn the_keychain_tier_serves_before_the_literal() {
        use crate::secrets::VolatileSecretStore;
        let mut profile = Profile::default();
        profile.upsert_instance(
            "anthropic",
            ProviderInstance {
                provider: "anthropic".into(),
                api_key: Some("file-key".into()),
                secret_ref: Some("ref-1".into()),
                base_url: None,
            },
        );
        let key_of = |profile: &Profile, secrets: Option<&dyn SecretStore>| {
            profile.stored_instance_key(
                profile.instance_for_provider("anthropic").unwrap().1,
                secrets,
            )
        };
        let store = VolatileSecretStore::new();
        // No entry yet: the literal serves.
        assert_eq!(key_of(&profile, Some(&store)).as_deref(), Some("file-key"));
        // The entry lands: the keychain outranks the literal.
        store.set("ref-1", "chain-key").unwrap();
        assert_eq!(key_of(&profile, Some(&store)).as_deref(), Some("chain-key"));
        // An unreachable store reads as "not in the store": the literal
        // serves, nothing is lost.
        assert_eq!(
            key_of(&profile, Some(&UnreachableStore)).as_deref(),
            Some("file-key")
        );
        // No store at all — the pre-keychain callers — same answer.
        assert_eq!(key_of(&profile, None).as_deref(), Some("file-key"));
    }

    #[test]
    fn place_key_prefers_the_chain_and_falls_back_to_the_file() {
        use crate::secrets::VolatileSecretStore;
        let mut profile = Profile {
            provider: Some("anthropic".into()),
            ..Profile::default()
        };
        profile.upsert_instance(
            "anthropic",
            ProviderInstance {
                provider: "anthropic".into(),
                api_key: None,
                secret_ref: None,
                base_url: None,
            },
        );
        // With a working store the key lives in the chain and the
        // instance carries only the ref.
        let store = VolatileSecretStore::new();
        assert_eq!(
            profile.place_key("anthropic", " sk-new ", Some(&store)),
            Some(KeyLocation::Keychain)
        );
        let instance = profile.instance_for_provider("anthropic").unwrap().1;
        assert_eq!(instance.api_key, None);
        let id = instance.secret_ref.clone().unwrap();
        assert_eq!(store.get(&id).unwrap().as_deref(), Some("sk-new"));
        assert_eq!(
            profile
                .stored_provider_key("anthropic", Some(&store))
                .as_deref(),
            Some("sk-new")
        );

        // Without a store the file keeps the key, trimmed.
        assert_eq!(
            profile.place_key("anthropic", " sk-file ", None),
            Some(KeyLocation::File)
        );
        let instance = profile.instance_for_provider("anthropic").unwrap().1;
        assert_eq!(instance.api_key.as_deref(), Some("sk-file"));

        // An unreachable store lands the key in the file too.
        assert_eq!(
            profile.place_key("anthropic", "sk-again", Some(&UnreachableStore)),
            Some(KeyLocation::File)
        );
        assert_eq!(
            profile
                .instance_for_provider("anthropic")
                .unwrap()
                .1
                .api_key
                .as_deref(),
            Some("sk-again")
        );
        // An unknown instance name places nothing.
        assert_eq!(profile.place_key("nope", "k", Some(&store)), None);
    }

    #[test]
    fn migration_moves_literals_into_the_store_and_round_trips() {
        use crate::secrets::VolatileSecretStore;
        let keyed = |provider: &str, key: &str| {
            let mut profile = Profile {
                provider: Some(provider.to_owned()),
                ..Profile::default()
            };
            profile.upsert_instance(
                provider,
                ProviderInstance {
                    provider: provider.to_owned(),
                    api_key: Some(key.to_owned()),
                    secret_ref: None,
                    base_url: None,
                },
            );
            profile
        };
        let mut config = Settings {
            profiles: BTreeMap::from([
                ("personal".to_owned(), keyed("google", "personal-key")),
                ("work".to_owned(), keyed("openrouter", "work-key")),
            ]),
            skills_disabled: BTreeSet::new(),
            disabled_mcp_servers: BTreeSet::new(),
        };
        let store = VolatileSecretStore::new();
        assert_eq!(config.migrate_secrets(&store), 2);
        for (name, expected) in [("google", "personal-key"), ("openrouter", "work-key")] {
            let instance = config
                .profiles
                .values()
                .flat_map(|p| p.instances.values())
                .find(|instance| instance.provider == name)
                .unwrap();
            assert_eq!(instance.api_key, None, "{name}'s literal is gone");
            let id = instance.secret_ref.as_deref().unwrap();
            assert_eq!(store.get(id).unwrap().as_deref(), Some(expected));
        }
        // Idempotent: nothing left to move.
        assert_eq!(config.migrate_secrets(&store), 0);
        // The refs survive the file round trip.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.yaml");
        Settings::save_to(&path, &config).unwrap();
        let mut loaded = Settings::load_from(&path).unwrap();
        assert_eq!(loaded, config);
        assert_eq!(loaded.migrate_secrets(&store), 0);
    }

    #[test]
    fn a_store_that_refuses_everything_keeps_the_literals() {
        let mut config = Settings::default();
        config.set_for_profile(Some("personal"), |p| {
            p.upsert_instance(
                "google",
                ProviderInstance {
                    provider: "google".into(),
                    api_key: Some("stays-put".into()),
                    secret_ref: None,
                    base_url: None,
                },
            );
        });
        assert_eq!(config.migrate_secrets(&UnreachableStore), 0);
        let instance = config.profiles["personal"]
            .instance_for_provider("google")
            .unwrap()
            .1;
        assert_eq!(instance.api_key.as_deref(), Some("stays-put"));
        assert_eq!(instance.secret_ref, None);
    }

    /// A profile carrying one sandbox section, and nothing else.
    #[test]
    fn ecrou_persist_defaults_on_and_stores_an_explicit_off() {
        let profile = sandboxed_profile(SandboxSettings::default());
        assert!(profile.sandbox_persist(), "unset keeps rootfs state");

        let settings = SandboxSettings {
            persist: Some(false),
            ..SandboxSettings::default()
        };
        assert!(!sandboxed_profile(settings).sandbox_persist());
    }

    fn sandboxed_profile(sandbox: SandboxSettings) -> Profile {
        Profile {
            sandbox: Some(sandbox),
            ..Profile::default()
        }
    }

    #[test]
    fn a_profile_round_trips_with_a_full_sandbox_section() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.yaml");
        let config = Settings {
            profiles: BTreeMap::from([(
                "personal".to_owned(),
                sandboxed_profile(SandboxSettings {
                    kind: Some("http".into()),
                    url: Some("https://sandbox.internal:7331".into()),
                    token: Some("literal-token".into()),
                    token_ref: Some("ref-sandbox".into()),
                    host: Some("deploy@box".into()),
                    container: Some("rhema-dev".into()),
                    active: Some("dev-vm".into()),
                    instance: Some("dev-vm".into()),
                    vm: Some(true),
                    image: Some("alpine:3.23".into()),
                    persist: None,
                }),
            )]),
            ..Settings::default()
        };
        Settings::save_to(&path, &config).unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        for key in [
            "sandbox:",
            "kind: http",
            "url:",
            "token:",
            "token_ref:",
            "host:",
            "container:",
        ] {
            assert!(raw.contains(key), "missing `{key}` in yaml on disk: {raw}");
        }
        let loaded = Settings::load_from(&path).unwrap();
        assert_eq!(loaded, config);

        // A typo'd sandbox key is loud, the file-wide strictness.
        std::fs::write(
            &path,
            "profiles:\n  personal:\n    sandbox:\n      knd: docker\n",
        )
        .unwrap();
        let err =
            Settings::load_from(&path).expect_err("a typo'd sandbox key must not read as unset");
        assert!(err.to_string().contains("knd"), "names the key: {err}");
    }

    #[test]
    fn the_sandbox_env_names_follow_the_same_env_wins_convention() {
        // The env tier of the sandbox trio, injected the way the
        // provider keys are: a test setting the real vars would race
        // the run (and the edition forbids the unsafe write).
        let env = |name: &str| match name {
            "RHEMA_SANDBOX" => Some("docker".into()),
            "RHEMA_SANDBOX_URL" => Some("  ".into()),
            "RHEMA_SANDBOX_TOKEN" => Some("env-token".into()),
            _ => None,
        };
        assert_eq!(
            resolve_key(env, &["RHEMA_SANDBOX"], Some("local".into())),
            Some("docker".into())
        );
        // A blank env value counts as unset: the stored value serves.
        assert_eq!(
            resolve_key(
                env,
                &["RHEMA_SANDBOX_URL"],
                Some("http://stored:7331".into())
            ),
            Some("http://stored:7331".into())
        );
        assert_eq!(
            resolve_key(env, &["RHEMA_SANDBOX_TOKEN"], Some("stored-token".into())),
            Some("env-token".into())
        );
        assert_eq!(resolve_key(|_| None, &["RHEMA_SANDBOX"], None), None);
    }

    #[test]
    fn the_sandbox_fields_read_back_through_the_profile() {
        // The public methods reach the real environment only when the
        // suite does not export the vars itself; the pure resolver
        // test above carries the env-wins coverage.
        if ["RHEMA_SANDBOX", "RHEMA_SANDBOX_URL", "RHEMA_SANDBOX_TOKEN"]
            .iter()
            .any(|name| std::env::var_os(name).is_some())
        {
            return;
        }
        let profile = sandboxed_profile(SandboxSettings {
            kind: Some(" ssh ".into()),
            url: Some(" http://box:7331 ".into()),
            token: Some(" literal ".into()),
            token_ref: Some("ref-sandbox".into()),
            host: Some(" deploy@box ".into()),
            container: Some(" rhema-dev ".into()),
            active: None,
            instance: Some(" dev-vm ".into()),
            vm: Some(true),
            image: Some(" alpine:3.23 ".into()),
            persist: None,
        });
        assert_eq!(profile.sandbox_kind().as_deref(), Some("ssh"));
        assert_eq!(profile.sandbox_url().as_deref(), Some("http://box:7331"));
        assert_eq!(profile.sandbox_token(None).as_deref(), Some("literal"));
        assert_eq!(profile.sandbox_image().as_deref(), Some("alpine:3.23"));
        assert_eq!(profile.sandbox_host().as_deref(), Some("deploy@box"));
        assert_eq!(profile.sandbox_container().as_deref(), Some("rhema-dev"));

        // A profile without the section resolves to nothing anywhere.
        let bare = Profile::default();
        assert_eq!(bare.sandbox_kind(), None);
        assert_eq!(bare.sandbox_url(), None);
        assert_eq!(bare.sandbox_token(None), None);
        assert_eq!(bare.sandbox_host(), None);
        assert_eq!(bare.sandbox_container(), None);

        // Blank stored values count as unset — host and container too,
        // which have no env tier to fall through to.
        let blank = sandboxed_profile(SandboxSettings {
            kind: Some("  ".into()),
            url: Some(" ".into()),
            host: Some("  ".into()),
            container: Some("".into()),
            ..SandboxSettings::default()
        });
        assert_eq!(blank.sandbox_kind(), None);
        assert_eq!(blank.sandbox_url(), None);
        assert_eq!(blank.sandbox_host(), None);
        assert_eq!(blank.sandbox_container(), None);
    }

    #[test]
    fn the_sandbox_token_tiers_run_keychain_then_literal() {
        use crate::secrets::VolatileSecretStore;
        let profile = sandboxed_profile(SandboxSettings {
            kind: Some("http".into()),
            url: Some("https://box:7331".into()),
            token: Some("file-token".into()),
            token_ref: Some("ref-sandbox".into()),
            ..SandboxSettings::default()
        });
        let store = VolatileSecretStore::new();
        // No entry yet: the literal serves.
        assert_eq!(
            profile.stored_sandbox_token(Some(&store)).as_deref(),
            Some("file-token")
        );
        // The entry lands: the keychain outranks the literal. (The env
        // tier outranks both — the resolver test above.)
        store.set("ref-sandbox", "chain-token").unwrap();
        assert_eq!(
            profile.stored_sandbox_token(Some(&store)).as_deref(),
            Some("chain-token")
        );
        // An unreachable store reads as "not in the store": the
        // literal serves, nothing is lost.
        assert_eq!(
            profile
                .stored_sandbox_token(Some(&UnreachableStore))
                .as_deref(),
            Some("file-token")
        );
        // A blank keychain entry counts as unset: the literal serves.
        store.set("ref-sandbox", "  ").unwrap();
        assert_eq!(
            profile.stored_sandbox_token(Some(&store)).as_deref(),
            Some("file-token")
        );
        // No store at all — the pre-keychain callers — same answer.
        assert_eq!(
            profile.stored_sandbox_token(None).as_deref(),
            Some("file-token")
        );
        // No section, no token, whatever the store holds.
        store.set("ref-sandbox", "chain-token").unwrap();
        assert_eq!(Profile::default().stored_sandbox_token(Some(&store)), None);
    }

    #[test]
    fn home_points_every_convention_at_its_root() {
        let home = Home::at("/home/u".into(), "/home/u/.godcode".into());
        assert_eq!(
            home.config_file(),
            PathBuf::from("/home/u/.godcode/config.yaml")
        );
        assert_eq!(home.skills(), PathBuf::from("/home/u/.godcode/skills"));
        assert_eq!(
            home.subagents(),
            PathBuf::from("/home/u/.godcode/subagents")
        );
        assert_eq!(
            home.system_prompt(),
            PathBuf::from("/home/u/.godcode/SYSTEM.md")
        );
        // The cross-agent convention stays machine-global: every
        // frontend shares `~/.agents/skills`, whatever its config base.
        assert_eq!(
            home.agents_skills(),
            PathBuf::from("/home/u/.agents/skills")
        );
    }

    #[test]
    fn settings_round_trip_through_a_foreign_store() {
        // The seam a second frontend implements: the section moves in
        // and out of any document, here a JSON envelope — the
        // framework never sees the format.
        struct GodCodeStore(std::sync::Mutex<String>);

        impl Store for GodCodeStore {
            fn load(&self) -> Result<Settings, ConfigError> {
                let doc = self
                    .0
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if doc.is_empty() {
                    return Ok(Settings::default());
                }
                let envelope: serde_json::Value = serde_json::from_str(&doc)
                    .map_err(|err| ConfigError::Store(err.to_string()))?;
                serde_json::from_value(envelope.get("rhema").cloned().unwrap_or_default())
                    .map_err(|err| ConfigError::Store(err.to_string()))
            }

            fn save(&self, settings: &Settings) -> Result<(), ConfigError> {
                let envelope = serde_json::json!({ "rhema": settings, "accent": "#00ff00" });
                *self
                    .0
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) =
                    serde_json::to_string(&envelope)
                        .map_err(|err| ConfigError::Store(err.to_string()))?;
                Ok(())
            }
        }

        let store = GodCodeStore(std::sync::Mutex::new(String::new()));
        // An empty document is the default settings.
        assert_eq!(store.load().unwrap(), Settings::default());

        let mut settings = Settings::default();
        settings.set_for_profile(Some("work"), |p| p.model = Some("m".into()));
        store.save(&settings).unwrap();
        // The frontend's own key rode along, untouched by the reload.
        let raw = store
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        assert!(raw.contains("accent"));
        assert_eq!(store.load().unwrap(), settings);
    }

    #[test]
    fn the_yaml_store_creates_the_directory_and_keeps_the_file_private() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("config.yaml");
        YamlStore::at(&path).save(&Settings::default()).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.path().join("nested"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o700, "the directory must be 0700");
        }
        assert_eq!(YamlStore::at(&path).load().unwrap(), Settings::default());
    }
}
