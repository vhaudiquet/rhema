//! The stored instance types a profile keys: one provider, web
//! search, decision, or sandbox instance, and the shared key-tier
//! resolution the four kinds read through.

use crate::catalog::{decision_env_names, websearch_env_names};
use crate::secrets::SecretStore;
use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};

/// Debug helper for credential fields: `Some("[…]")` when a value is
/// present, `None` otherwise, presence without the value, so a
/// `{:?}` of any config shape can never print a live key.
fn redacted(value: &Option<String>) -> Option<&'static str> {
    value.as_ref().map(|_| "[…]")
}
/// Which request shape a provider instance speaks, when the choice is
/// the user's to make. Today only the `openai` kind reads it, the one
/// kind whose dialect its route alone cannot imply: OpenAI itself
/// serves the Responses API, an arbitrary OpenAI-compatible server
/// only promises chat completions, and the stored choice overrides the
/// route rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderApi {
    /// The Chat Completions API (`POST {base}/chat/completions`), the
    /// dialect every OpenAI-compatible server answers.
    Chat,
    /// The Responses API (`POST {base}/responses`), the modern shape,
    /// where a tool result's images can ride the result itself.
    Responses,
}

impl ProviderApi {
    /// The config spelling, matching the serde form.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Chat => "chat",
            Self::Responses => "responses",
        }
    }
}
/// One logged-in provider instance: a named credential set for a single
/// provider kind. A profile may hold any number of them, two llama.cpp
/// servers, two OpenAI-compatible endpoints, a work and a personal
/// Copilot sub, keyed by a user-chosen name; the instance's `provider`
/// says which adapter speaks for it.
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "snake_case", deny_unknown_fields)]
pub struct ProviderInstance {
    /// Which adapter serves this instance: `google`, `llama-cpp`,
    /// `openrouter`, `anthropic`, `openai`, `deepseek`, `zai`,
    /// `opencode`, `copilot`, `chatgpt`, `mistral`, or `mistral-vibe`.
    pub provider: String,
    /// The API key (the bearer key for a llama.cpp server), when the
    /// instance needs one and the secret store does not hold it.
    /// Environment variables still win at use time.
    pub api_key: Option<String>,
    /// The secret-store entry holding the key, when it is not in this
    /// file: an opaque id under the frontend's secret-store file,
    /// resolved through the [`crate::secrets`] seam. Written by
    /// [`crate::profile::Profile::place_key`] and [`crate::settings::Settings::migrate_secrets`]; a
    /// literal `api_key` and a live `secret_ref` never hold the same
    /// key at rest.
    pub secret_ref: Option<String>,
    /// The server base URL: the llama.cpp address, an OpenAI-compatible
    /// endpoint's base, or a Z.ai route override. Unset means the
    /// provider's own default.
    pub base_url: Option<String>,
    /// The request shape the instance speaks, when the user chose one:
    /// `chat` or `responses`. Unset lets the provider's route rule
    /// decide (OpenAI itself: responses; a custom endpoint: chat
    /// completions; Mistral: responses, the pin a deployment
    /// without the route sets back to chat completions).
    pub api: Option<ProviderApi>,
}
/// The two fields the literal-to-store migration touches, so the
/// three instance kinds (provider, web search, decision) share one
/// loop body instead of three copies.
pub(crate) trait KeyedInstance {
    /// The literal `api_key`, when the file holds one.
    fn literal_key(&self) -> Option<String>;
    /// The secret-store entry's id, when one is referenced.
    fn secret_ref(&self) -> Option<String>;
    /// Records a successful move: the id is the key's new home, the
    /// literal is gone.
    fn set_moved(&mut self, id: String);
}

impl KeyedInstance for ProviderInstance {
    fn literal_key(&self) -> Option<String> {
        self.api_key.clone()
    }
    fn secret_ref(&self) -> Option<String> {
        self.secret_ref.clone()
    }
    fn set_moved(&mut self, id: String) {
        self.secret_ref = Some(id);
        self.api_key = None;
    }
}

impl KeyedInstance for WebSearchInstance {
    fn literal_key(&self) -> Option<String> {
        self.api_key.clone()
    }
    fn secret_ref(&self) -> Option<String> {
        self.secret_ref.clone()
    }
    fn set_moved(&mut self, id: String) {
        self.secret_ref = Some(id);
        self.api_key = None;
    }
}

impl KeyedInstance for DecisionInstance {
    fn literal_key(&self) -> Option<String> {
        self.api_key.clone()
    }
    fn secret_ref(&self) -> Option<String> {
        self.secret_ref.clone()
    }
    fn set_moved(&mut self, id: String) {
        self.secret_ref = Some(id);
        self.api_key = None;
    }
}
/// One literal-to-store move, the body every `migrate_secrets`
/// loop shares: a blank key moves nothing, a secret store that refuses
/// leaves the literal in place, a secret store that accepts leaves the
/// `secret_ref`. Returns 1 when a key moved.
pub(crate) fn migrate_instance_key(
    instance: &mut impl KeyedInstance,
    secrets: &dyn SecretStore,
) -> usize {
    let Some(key) = instance
        .literal_key()
        .map(|key| key.trim().to_owned())
        .filter(|key| !key.is_empty())
    else {
        return 0;
    };
    let id = instance
        .secret_ref()
        .clone()
        .unwrap_or_else(crate::secrets::fresh_id);
    if secrets.set(&id, &key).is_ok() {
        instance.set_moved(id);
        1
    } else {
        0
    }
}
impl fmt::Debug for ProviderInstance {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProviderInstance")
            .field("provider", &self.provider)
            .field("api_key", &redacted(&self.api_key))
            .field("secret_ref", &self.secret_ref)
            .field("base_url", &self.base_url)
            .field("api", &self.api)
            .finish()
    }
}
/// One web search provider, a user-chosen name over a kind and a
/// key, the [`ProviderInstance`] twin without the routing fields:
/// search vendors have no base url to override and no request shape
/// to pick in v1. Stored per profile under [`crate::profile::Profile::websearch`],
/// tried in [`crate::profile::Profile::websearch_order`].
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "snake_case", deny_unknown_fields)]
pub struct WebSearchInstance {
    /// Which adapter serves this instance: `brave`, `exa`, `tavily`,
    /// or `opencode`. Stored as given, unknown spellings are the
    /// consumer's to resolve or reject (the registry does, at use
    /// time, not load time).
    pub provider: String,
    /// The API key, when the secret store does not hold it. Environment
    /// variables still win at use time.
    pub api_key: Option<String>,
    /// The secret-store entry holding the key, when it is not in this
    /// file: an opaque id under the frontend's secret-store file,
    /// resolved through the [`crate::secrets`] seam. Written by
    /// [`crate::profile::Profile::place_websearch_key`] and
    /// [`crate::settings::Settings::migrate_secrets`]; a literal `api_key` and a live
    /// `secret_ref` never hold the same key at rest.
    pub secret_ref: Option<String>,
}

impl fmt::Debug for WebSearchInstance {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WebSearchInstance")
            .field("provider", &self.provider)
            .field("api_key", &redacted(&self.api_key))
            .field("secret_ref", &self.secret_ref)
            .finish()
    }
}

impl WebSearchInstance {
    /// The key this instance serves: the provider kind's environment
    /// variables first, then the stored key, secret store, then literal.
    /// Whitespace-only values count as unset at every tier, the
    /// provider-instance rule.
    pub fn api_key(&self, secrets: Option<&dyn SecretStore>) -> Option<String> {
        resolve_key(
            |name| std::env::var(name).ok(),
            websearch_env_names(&self.provider),
            stored_websearch_key(self, secrets),
        )
    }
}

/// The stored key of one web search instance without the environment
/// tier: the secret-store entry `secret_ref` names, then the literal. A
/// store that cannot be reached reads as "not in the store", the
/// next tier serves, never a lost credential.
fn stored_websearch_key(
    instance: &WebSearchInstance,
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
/// The environment variable naming the Cloudflare account a Clef
/// instance runs on. The account id is not a secret, but it rides the
/// same tiers as the key so a CI machine sets one pair of variables
/// and nothing else (`specs/rhema-decision.md` §3).
pub const CLOUDFLARE_ACCOUNT_ENV: &str = "CLOUDFLARE_ACCOUNT_ID";

/// One decision model instance: which vendor, which of its models,
/// and the key, the [`ProviderInstance`] twin with the two fields the
/// decision vendors need (a model, an account id). A profile's
/// [`DecisionSettings`] holds any number, a Jev key and a work and a
/// personal Clef account, a vendor and a self-hosted System One
/// server, an OpenRouter key beside the vendor's own, and `decide`
/// asks the one the section's `instance` names, with no fallback from
/// one vendor to another.
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "snake_case", deny_unknown_fields)]
pub struct DecisionInstance {
    /// Which adapter serves this instance: `jev` (TypeSafe),
    /// `clef` (Cloudflare Workers AI) or `openrouter` (the
    /// aggregator's Decisions route, which serves both and forty-odd
    /// other System One models under one key). Stored as given,
    /// unknown spellings are the consumer's to resolve or reject (the
    /// registry does, at use time, not load time).
    pub provider: String,
    /// The API key (Jev, OpenRouter) or Workers AI API token (Clef),
    /// when the secret store does not hold it. Environment variables
    /// still win at use time.
    pub api_key: Option<String>,
    /// The secret-store entry holding the key, when it is not in this
    /// file: an opaque id under the frontend's secret-store file,
    /// resolved through the [`crate::secrets`] seam. Written by
    /// [`crate::profile::Profile::place_decision_key`] and
    /// [`crate::settings::Settings::migrate_secrets`]; a literal `api_key` and a live
    /// `secret_ref` never hold the same key at rest. Two instances
    /// may share one `secret_ref`: one Jev key, `jev-latest` and
    /// `jev-preview` as two instances to compare.
    pub secret_ref: Option<String>,
    /// Which of the vendor's models to ask. `jev-latest` (the moving
    /// alias) when unset for Jev; `jev-preview` or a pinned version to
    /// compare runs. `clef` (the 27B model, the default) or
    /// `clef-flash` (the 9B one) for Clef, where the model is also the
    /// route. A slug for OpenRouter, `typesafe/jev-1.14` when unset;
    /// any model its catalog lists under `output_modalities=decisions`
    /// (`cloudflare/clef-flash`, `~typesafe/jev-latest`, a `:free`
    /// one) goes.
    pub model: Option<String>,
    /// The Cloudflare account id, read by Clef only: the account the
    /// model runs on. [`CLOUDFLARE_ACCOUNT_ENV`] wins at use time.
    pub account_id: Option<String>,
    /// The API root to call instead of the vendor's: a self-hosted
    /// System One server for Jev (Ollaya, vLLM, SGLang, llama.cpp all
    /// serve `/v1/systemone`), the Workers AI root for Clef, the
    /// OpenRouter root for an OpenRouter instance. Unset means the
    /// vendor's own.
    pub base_url: Option<String>,
}

impl fmt::Debug for DecisionInstance {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DecisionInstance")
            .field("provider", &self.provider)
            .field("api_key", &redacted(&self.api_key))
            .field("secret_ref", &self.secret_ref)
            .field("model", &self.model)
            .field("account_id", &self.account_id)
            .field("base_url", &self.base_url)
            .finish()
    }
}

impl DecisionInstance {
    /// The key this instance serves: the vendor kind's environment
    /// variables first, then the stored key, secret store, then literal.
    /// Whitespace-only values count as unset at every tier, the
    /// provider-instance rule.
    pub fn api_key(&self, secrets: Option<&dyn SecretStore>) -> Option<String> {
        resolve_key(
            |name| std::env::var(name).ok(),
            decision_env_names(&self.provider),
            stored_decision_key(self, secrets),
        )
    }

    /// The account this instance runs on, Clef only:
    /// [`CLOUDFLARE_ACCOUNT_ENV`] when set, else the stored id.
    /// Whitespace-only values count as unset.
    pub fn account_id(&self) -> Option<String> {
        resolve_key(
            |name| std::env::var(name).ok(),
            &[CLOUDFLARE_ACCOUNT_ENV],
            self.account_id.clone(),
        )
    }
}
/// The stored key of one decision instance without the environment
/// tier: the secret-store entry `secret_ref` names, then the literal. A
/// store that cannot be reached reads as "not in the store", the
/// next tier serves, never a lost credential.
fn stored_decision_key(
    instance: &DecisionInstance,
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
/// The profile's decision models, the `decision` section: the named
/// instances and the one `decide` asks by default, the turn
/// providers' shape (`instances` plus `instance`) under one heading.
/// [`crate::profile::Profile::active_decision_instance`] is the rule, the
/// [`crate::profile::Profile::active_instance`] twin with the kind riding the
/// instance. Keys are secrets at rest and redact in [`Debug`].
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "snake_case", deny_unknown_fields)]
pub struct DecisionSettings {
    /// The name of the default instance in [`DecisionSettings::instances`],
    /// the one `decide` asks. Must name an instance to count; unset
    /// or dangling, the first instance by name is the default, the
    /// [`crate::profile::Profile::instance`] rule. Absent from the file while unset.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instance: Option<String>,
    /// The instances, keyed by a user-chosen name: the
    /// [`ProviderInstance`] twin with a model and, for Clef, an
    /// account id. Two may share a `secret_ref` (one key, two models
    /// to compare). Absent from the file while empty; a profile
    /// holding none is served by the environment tier.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub instances: BTreeMap<String, DecisionInstance>,
}

impl DecisionSettings {
    /// The instance asked by default: the one [`Self::instance`]
    /// names when it exists, else the first by name. `None` when the
    /// section holds no instance.
    pub fn active_instance(&self) -> Option<(&str, &DecisionInstance)> {
        let name = match self.instance.as_deref() {
            Some(name) if self.instances.contains_key(name) => name,
            _ => self.instances.keys().next()?,
        };
        Some((name, &self.instances[name]))
    }
}
/// One named place `bash` can run: a user-chosen name over a kind and
/// whatever that kind needs, the sandbox twin of
/// [`ProviderInstance`]. `local` needs no record: the local instance
/// exists by doing nothing.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SandboxInstance {
    /// Which kind runs here: `docker`, `lxd`, `ssh`, `http`, or
    /// `ecrou`. Stored as given, unknown spellings are the
    /// consumer's to resolve or reject.
    pub kind: String,
    /// What the kind names its instance by: the docker container, the
    /// LXD instance, the ssh host, or the http URL. Whitespace-only
    /// values count as unset at use time.
    pub target: Option<String>,
    /// Whether an `lxd` instance is a virtual machine, the launch
    /// flag the assembly stands it up with. Unset is a container, and
    /// every other kind ignores it.
    pub vm: Option<bool>,
}
/// Where `bash` runs when a frontend honors the stored choice: the
/// [`crate::profile::Profile`] section behind the sandbox kinds `local`, `docker`,
/// `ssh`, and `http`. Every field is stored as a string and validated
/// by the consumer, the same stance as every stored name:
/// rhema-config stores, the frontend parses. A profile
/// without this section and a `kind: local` section are the same
/// behavior: today's local process executor.
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "snake_case", deny_unknown_fields)]
pub struct SandboxSettings {
    /// The stored choice: `local`, `docker`, `lxd`, `ssh`, `http`, or
    /// `ecrou`. The name is stored as given, unknown spellings are
    /// the consumer's to resolve or reject.
    pub kind: Option<String>,
    /// The name of the active sandbox instance in
    /// [`crate::profile::Profile::sandbox_instances`], the one `bash` runs in. Must
    /// name an instance of `kind` to count; `None` is the section's
    /// own single-instance form (the facts below), the shape this map
    /// grew out of.
    pub active: Option<String>,
    /// The `http` sandbox's base URL.
    pub url: Option<String>,
    /// A literal bearer token for the `http` sandbox, the fallback
    /// where no secret store answers, never the first choice.
    pub token: Option<String>,
    /// The secret-store entry holding the `http` token, resolved through
    /// the [`crate::secrets`] seam, [`ProviderInstance`]'s `secret_ref`
    /// pattern. A literal `token` and a live `token_ref` never hold
    /// the same secret at rest.
    pub token_ref: Option<String>,
    /// The `ssh` destination: `user@host` or an ssh alias, passed to
    /// `ssh` as-is.
    pub host: Option<String>,
    /// The `docker` container to exec in.
    pub container: Option<String>,
    /// The `lxd` instance to exec in, container or virtual machine,
    /// the type riding `vm` (spec §9: one driver and a launch flag,
    /// not two kinds).
    pub instance: Option<String>,
    /// Whether the `lxd` instance is a virtual machine, the launch
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
    /// Whether the host resolves the search and decision providers and
    /// hands their keys to the sandbox's helper. Off by default: a key
    /// in the sandbox is readable by every command the model runs, so
    /// turning it on is a deliberate trust decision.
    pub inject_credentials: bool,
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
/// Pure key resolution: the first non-blank of the named environment
/// variables, then `stored`; whitespace-only values count as unset.
pub(crate) fn resolve_key(
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
pub(crate) mod tests;
