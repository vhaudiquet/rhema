//! One profile: the stored section a session's shape depends on.

use crate::catalog::{KeyLocation, provider_env_names};
use crate::instances::{
    DecisionInstance, DecisionSettings, ProviderInstance, SandboxInstance, SandboxSettings,
    WebSearchInstance, resolve_key,
};
use crate::secrets::SecretStore;
use std::collections::BTreeMap;
use std::fmt;

use rhema_core::request::ThinkingEffort;
use serde::{Deserialize, Serialize};

/// One named set of provider settings. Profiles are isolated: work never
/// inherits personal's model or keys, and vice versa.
///
/// Credentials live in [`Profile::instances`], named
/// [`ProviderInstance`]s. Unknown keys are rejected: a typo'd `model`
/// must not silently read as unset.
#[derive(Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "snake_case", deny_unknown_fields)]
pub struct Profile {
    /// Which provider serves turns: `google`, `llama-cpp`, `openrouter`,
    /// `anthropic`, `openai`, `deepseek`, `zai`, `opencode`,
    /// `copilot`, `chatgpt`, `mistral`, or `mistral-vibe`.
    pub provider: Option<String>,
    /// The name of the active instance in [`Profile::instances`], the
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
    /// all read as `medium`, a level is always named, because
    /// always-thinking models left to their defaults spend the whole
    /// output budget on reasoning and answer with nothing.
    pub thinking: Option<String>,
    /// Where `bash` runs when the frontend honors the choice: the
    /// kind plus whatever that kind needs. `None` and `kind: local`
    /// are the same behavior; the choice machinery engages only
    /// non-local kinds.
    pub sandbox: Option<SandboxSettings>,
    /// The named places `bash` can run, keyed by a user-chosen name,
    /// the sandbox twin of [`Profile::instances`]. Absent from the
    /// file while empty: most profiles never leave `local`.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub sandbox_instances: BTreeMap<String, SandboxInstance>,
    /// The MCP servers this profile exposes to its agents, keyed by a
    /// user-chosen name matching `[a-z0-9_-]+`. Per profile by
    /// design: work and personal carry different servers, and a
    /// server installed through one profile's menu appears nowhere
    /// else. Resolved through [`crate::mcp::McpConfig::from_entries`]
    /// at agent-wiring time, a malformed entry is skipped with a
    /// notice, never a config failure. Absent from the file while
    /// empty; entries' `env` and `headers` values are secrets at rest
    /// and redact in [`Debug`].
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub mcp_servers: BTreeMap<String, crate::mcp::McpServerEntry>,
    /// The web search providers this profile's agents reach, keyed by
    /// a user-chosen name, the [`ProviderInstance`] twin at search
    /// scale. Per profile by design, work and personal carry
    /// different vendors and keys. Absent from the file while empty;
    /// entries' keys are secrets at rest and redact in [`Debug`].
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub websearch: BTreeMap<String, WebSearchInstance>,
    /// The fallback order over [`Profile::websearch`], the chain the
    /// search macro walks: first success wins, an entry naming no
    /// instance is skipped, an instance missing from the order is
    /// inert. Absent from the file while empty.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub websearch_order: Vec<String>,
    /// The decision models this profile's agents may ask through
    /// `decide`: the named instances and the default, the
    /// [`DecisionSettings`] section. Per profile by design, work and
    /// personal carry different vendors and keys. Absent from the
    /// file while unset, and then the environment tier serves
    /// (`TYPESAFE_API_KEY`, `CLOUDFLARE_AUTH_TOKEN` plus
    /// `CLOUDFLARE_ACCOUNT_ID`, or `OPENROUTER_API_KEY`, the first
    /// that resolves); a present section holding no instance is served
    /// the same way.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decision: Option<DecisionSettings>,
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
            .field("websearch", &self.websearch)
            .field("websearch_order", &self.websearch_order)
            .field("decision", &self.decision)
            .finish()
    }
}

impl Profile {
    /// The provider this profile serves turns on: `RHEMA_PROVIDER`, then
    /// the stored `provider`. Whitespace-only values count as unset. The
    /// name is returned as given, unknown spellings are the consumer's
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
    /// instance of it. The provider name is the adapter spelling,
    /// `google`, `llama-cpp`, `openrouter`, `anthropic`, `openai`,
    /// `deepseek`, `zai`, `opencode`, `copilot`, `chatgpt`, `mistral`,
    /// `mistral-vibe`.
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
    /// Only an explicit `default` keeps the provider's own behavior, a
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
        // The effective provider, `RHEMA_PROVIDER` included: the stored
        // spelling alone would call a profile unusable when an
        // environment override supplies exactly the credential it
        // needs.
        let Some(provider) = self.provider_name() else {
            return false;
        };
        match provider.as_str() {
            "llama-cpp" => true,
            "openrouter" => self.api_key("openrouter", secrets).is_some(),
            "anthropic" => self.api_key("anthropic", secrets).is_some(),
            "openai" => self.api_key("openai", secrets).is_some(),
            "deepseek" => self.api_key("deepseek", secrets).is_some(),
            "zai" => self.api_key("zai", secrets).is_some(),
            "opencode" => self.api_key("opencode", secrets).is_some(),
            "copilot" => self.api_key("copilot", secrets).is_some(),
            "chatgpt" => self.api_key("chatgpt", secrets).is_some(),
            "mistral" => self.api_key("mistral", secrets).is_some(),
            "mistral-vibe" => self.api_key("mistral-vibe", secrets).is_some(),
            "google" => self.api_key("google", secrets).is_some(),
            // An unknown spelling never reads as google.
            _ => false,
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
    /// `provider`/`instance` coherent, a new active instance must be
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
    /// else `None`, the section's own facts (or the derived name) are
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
    /// keeps `sandbox.kind`/`sandbox.active` coherent, a new active
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
    /// store that cannot be reached reads as "not in the store", the
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
    /// first, then the stored key, keychain, then literal.
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
    /// when the store accepts it, the instance then carries only its
    /// `secret_ref`, else the literal, the shape machines without a
    /// store have always held. `None` when no instance bears `name`.
    pub fn place_key(
        &mut self,
        name: &str,
        key: &str,
        secrets: Option<&dyn SecretStore>,
    ) -> Option<KeyLocation> {
        let instance = self.instances.get_mut(name)?;
        Some(Self::place_keyed(
            key,
            secrets,
            &mut instance.api_key,
            &mut instance.secret_ref,
        ))
    }

    /// The profile's web search chain: the instances of
    /// [`Profile::websearch`] in [`Profile::websearch_order`], one
    /// `(name, instance)` pair per run, entries naming no instance
    /// skipped, instances missing from the order inert. The order is
    /// the whole chain, not a priority hint; the caller resolves each
    /// instance's key with [`WebSearchInstance::api_key`].
    pub fn websearch_chain(&self) -> Vec<(String, &WebSearchInstance)> {
        self.websearch_order
            .iter()
            .filter_map(|name| self.websearch.get(name).map(|entry| (name.clone(), entry)))
            .collect()
    }

    /// Places a just-entered key on the named web search instance:
    /// the keychain, when the store accepts it, the instance then
    /// carries only its `secret_ref`, else the literal. `None` when
    /// no instance bears `name`.
    pub fn place_websearch_key(
        &mut self,
        name: &str,
        key: &str,
        secrets: Option<&dyn SecretStore>,
    ) -> Option<KeyLocation> {
        let instance = self.websearch.get_mut(name)?;
        Some(Self::place_keyed(
            key,
            secrets,
            &mut instance.api_key,
            &mut instance.secret_ref,
        ))
    }

    /// The decision instance `decide` asks by default:
    /// [`DecisionSettings::active_instance`] on the profile's
    /// section, the [`Profile::active_instance`] rule with the kind
    /// riding the instance instead of standing beside it. `None`
    /// when the profile holds no instance, and then the environment
    /// tier serves.
    pub fn active_decision_instance(&self) -> Option<(&str, &DecisionInstance)> {
        self.decision.as_ref()?.active_instance()
    }

    /// Places a just-entered key on any keyed instance, the one body
    /// the three instance kinds share
    /// ([`Self::place_key`], [`Self::place_websearch_key`], and
    /// [`Self::place_decision_key`], which differ only in the map
    /// they name the instance in): the keychain, when the store
    /// accepts it, the instance then carries only its `secret_ref`,
    /// else the literal, the shape machines without a store have
    /// always held.
    ///
    /// A blank key clears both tiers, deleting the keychain entry a
    /// stale `secret_ref` would otherwise keep live; a store that
    /// refuses leaves the literal in place and drops the old ref
    /// with its entry so `stored_instance_key` never prefers a key
    /// the user just replaced. Nothing is lost either way.
    fn place_keyed(
        key: &str,
        secrets: Option<&dyn SecretStore>,
        api_key: &mut Option<String>,
        secret_ref: &mut Option<String>,
    ) -> KeyLocation {
        let trimmed = key.trim();
        if trimmed.is_empty() {
            // Clearing removes both tiers: a stale `secret_ref` would
            // otherwise keep the old keychain value live.
            if let (Some(store), Some(id)) = (secrets, secret_ref.take()) {
                let _ = store.delete(&id);
            }
            *api_key = None;
            return KeyLocation::File;
        }
        if secrets.is_none() {
            // No store to fall back from: the literal is the key, and
            // a stale ref must not shadow it later.
            *secret_ref = None;
            *api_key = Some(trimmed.to_owned());
            return KeyLocation::File;
        }
        let store = secrets.expect("checked above");
        let id = secret_ref.clone().unwrap_or_else(crate::secrets::fresh_id);
        match store.set(&id, trimmed) {
            Ok(()) => {
                *secret_ref = Some(id);
                *api_key = None;
                KeyLocation::Keychain
            }
            // The keychain refused: the file keeps the key, the
            // behavior it has always had. Nothing is lost.
            Err(_) => {
                // Drop the old ref with its entry: keeping it would
                // make `stored_instance_key` prefer a key the user
                // just replaced.
                if let Some(old) = secret_ref.take() {
                    let _ = store.delete(&old);
                }
                *api_key = Some(trimmed.to_owned());
                KeyLocation::File
            }
        }
    }

    /// Places a just-entered key on the named decision instance:
    /// the keychain, when the store accepts it, the instance then
    /// carries only its `secret_ref`, else the literal,
    /// [`Profile::place_key`]'s rule. `None` when no instance bears
    /// `name`.
    pub fn place_decision_key(
        &mut self,
        name: &str,
        key: &str,
        secrets: Option<&dyn SecretStore>,
    ) -> Option<KeyLocation> {
        let instance = self.decision.as_mut()?.instances.get_mut(name)?;
        Some(Self::place_keyed(
            key,
            secrets,
            &mut instance.api_key,
            &mut instance.secret_ref,
        ))
    }

    /// The base URL an instance serves: `LLAMA_CPP_URL` over the stored
    /// URL for llama.cpp; the stored base for OpenAI-compatible, Z.ai,
    /// and OpenCode instances (the caller defaults each kind, so its own
    /// default stays in one place); `None` for key-only providers.
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
            "openai" | "zai" | "opencode" => instance
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
    /// The name is returned as given, `local`, `docker`, `ssh`,
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
    /// store", the next tier serves, never a lost token. Callers that
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

    /// The `ssh` destination as stored, `user@host` or an ssh alias,
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

    /// The `docker` container to exec in, as stored. Stored only, no
    /// environment tier, the same stance as [`Profile::sandbox_host`].
    /// Whitespace-only values count as unset.
    pub fn sandbox_container(&self) -> Option<String> {
        self.sandbox
            .as_ref()
            .and_then(|s| s.container.clone())
            .map(|c| c.trim().to_owned())
            .filter(|c| !c.is_empty())
    }

    /// The `lxd` instance to exec in, as stored. Stored only, the
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

    /// The `ecrou` image every boot resolves, as stored. Stored only,
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
#[cfg(test)]
mod tests;
