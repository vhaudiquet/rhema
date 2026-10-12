//! The backend registry: every provider the deployment can speak to, one
//! active at a time, switchable at runtime.
//!
//! At boot the registry constructs every provider whose credentials
//! resolve — env vars win, the claw's own stored credentials serve
//! next, the stored rhema config after that, and the keyless
//! llama-cpp backend is always constructible, so a daemon can always
//! boot. The same construction runs over HTTP: credentials saved
//! through the surface insert (or replace) a backend in the live
//! registry and persist in the settings store. A backend is one
//! *instance*, named by its `id` — the kind unless a login named it,
//! so two llama.cpp servers coexist under two ids; ids of instances
//! discovered from env or the config are always their kind. One
//! backend is *active*; handlers read the registry per
//! dispatch, so a switch takes effect on the next turn without a
//! restart. The active pair is deployment state: seeded from the config
//! file, persisted in the settings store, changeable over `/v1/providers`
//! — the chart pins nothing.
//!
//! The per-kind default models are fallbacks only, in one place here:
//! openrouter's `openrouter/auto` deliberately replaces the old
//! copy-pasted google default — a router deserves its own router id.

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, PoisonError, RwLock, RwLockWriteGuard};

use rhema_autonomous_agent::async_trait;
use rhema_core::{EventSink, ModelOutput, ModelProvider, ModelRequest, ProviderError};
use rhema_provider_anthropic::AnthropicProvider;
use rhema_provider_copilot::CopilotProvider;
use rhema_provider_google::GoogleProvider;
use rhema_provider_llama_cpp::LlamaCppProvider;
use rhema_provider_openai_compatible::OpenAiCompatibleProvider;
use rhema_provider_openrouter::OpenRouterProvider;

use rhema_provider::ModelCard;
use serde::{Deserialize, Serialize};

/// The default model per backend kind, when neither the operator nor the
/// settings store names one.
pub fn default_model_for(kind: &str) -> &'static str {
    match kind {
        "llama-cpp" => "local",
        "anthropic" => "claude-sonnet-4-5",
        "openai" => "gpt-5",
        "openrouter" => "openrouter/auto",
        "deepseek" => "deepseek-chat",
        "copilot" => "gpt-4.1",
        _ => "gemini-3.8-flash",
    }
}

/// The listing of a card-backed backend, boxed for the registry; a
/// backend without one (the mock, llama's raw id list) carries `None`.
pub type ModelListing = Pin<Box<dyn Future<Output = Result<Vec<ModelCard>, ProviderError>> + Send>>;

/// One constructible backend instance: the provider seam, the id the
/// registry keys it under, its kind, its model fallback, and how to
/// list its models.
pub struct Backend {
    /// The instance id: the kind unless the login named it. The
    /// registry's key, the credentials row's suffix, and the active
    /// pair's first half are all this string.
    pub id: String,
    /// The backend kind: `"google"`, `"llama-cpp"`, `"openrouter"`,
    /// `"anthropic"`, `"openai"`, `"mock"`.
    pub kind: String,
    /// The provider behind the daemon's `ModelProvider` seam.
    pub provider: Arc<dyn ModelProvider>,
    /// The model used when none is set for this kind.
    pub default_model: String,
    /// The live model listing, when the backend has one worth calling.
    list_models: Option<Arc<dyn Fn() -> ModelListing + Send + Sync>>,
}

impl Backend {
    /// A backend without a live listing (the mock). The id doubles as
    /// the kind: the registry-injected kinds are singletons.
    pub fn headless(kind: impl Into<String>, provider: Arc<dyn ModelProvider>) -> Self {
        let kind = kind.into();
        Self {
            id: kind.clone(),
            kind,
            provider,
            default_model: "mock".to_owned(),
            list_models: None,
        }
    }
}

/// The currently active backend instance: id plus the model turns run on.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Active {
    id: String,
    model: String,
}

/// One row of the registry listing: the instance id, its kind, and the
/// model fallback.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackendRow {
    /// The instance id.
    pub id: String,
    /// The backend kind.
    pub kind: String,
    /// The model used when none is set for this instance.
    pub default_model: String,
}

/// The deployment's providers and the active pair, shared by every
/// handler so a switch takes effect on the next dispatch. The map is
/// keyed by instance id, not kind — two instances of one kind are two
/// rows.
pub struct BackendRegistry {
    backends: RwLock<BTreeMap<String, Arc<Backend>>>,
    active: RwLock<Active>,
}

impl BackendRegistry {
    /// A registry over `backends`. The active pair is `seed` when that
    /// id exists, else the first backend's id and default model. An
    /// empty list is a construction error: the registry exists so
    /// handlers never face an absent backend.
    pub fn new(
        backends: Vec<Backend>,
        seed: Option<(String, Option<String>)>,
    ) -> Result<Self, String> {
        if backends.is_empty() {
            return Err(
                "no provider backend is constructible: no key resolved for any provider".to_owned(),
            );
        }
        let map: BTreeMap<String, Arc<Backend>> = backends
            .into_iter()
            .map(|backend| (backend.id.clone(), Arc::new(backend)))
            .collect();
        let active = match seed {
            Some((id, model)) if map.contains_key(&id) => {
                let default = map[&id].default_model.clone();
                Active {
                    id,
                    model: model.unwrap_or(default),
                }
            }
            _ => {
                let first = map.values().next().expect("non-empty above");
                Active {
                    id: first.id.clone(),
                    model: first.default_model.clone(),
                }
            }
        };
        Ok(Self {
            backends: RwLock::new(map),
            active: RwLock::new(active),
        })
    }

    /// Inserts (or replaces) `backend` under its id in the live
    /// registry: the write behind the credentials routes. The active
    /// pair is untouched — a replaced backend keeps serving under its
    /// id.
    pub fn insert(&self, backend: Backend) {
        let mut backends = self.backends_lock();
        backends.insert(backend.id.clone(), Arc::new(backend));
    }

    /// The backend registered under `id`, when one is.
    pub fn get(&self, id: &str) -> Option<Arc<Backend>> {
        self.backends_lock().get(id).cloned()
    }

    /// Removes the instance `id` from the live registry. Removing the
    /// last backend is refused: handlers never face an absent backend.
    /// When the removed instance was active, the first remaining
    /// backend (ascending) takes over with its default model — the
    /// caller re-persists the pair it reads back from [`Self::active`].
    pub fn remove(&self, id: &str) -> Result<(), String> {
        let mut backends = self.backends_lock();
        if backends.len() == 1 && backends.contains_key(id) {
            return Err("cannot remove the last provider backend".to_owned());
        }
        let removed_was_active = self.active_lock().id == id;
        if backends.remove(id).is_none() {
            return Err(format!("unknown provider backend {id:?}"));
        }
        if removed_was_active {
            let first = backends.values().next().expect("a backend remains");
            *self.active_lock() = Active {
                id: first.id.clone(),
                model: first.default_model.clone(),
            };
        }
        Ok(())
    }

    /// The provider and model the next dispatch runs on.
    pub fn current(&self) -> (Arc<dyn ModelProvider>, String) {
        let active = self.active_lock();
        let backend = self.backends_lock()[&active.id].clone();
        (backend.provider.clone(), active.model.clone())
    }

    /// The active pair as data: instance id and model.
    pub fn active(&self) -> (String, String) {
        let active = self.active_lock();
        (active.id.clone(), active.model.clone())
    }

    /// Switches the active pair. An unknown instance id is an error; a
    /// model left unset falls back to that instance's default.
    pub fn set_active(&self, id: &str, model: Option<&str>) -> Result<(), String> {
        let default = {
            let backends = self.backends_lock();
            let backend = backends
                .get(id)
                .ok_or_else(|| format!("unknown provider backend {id:?}"))?;
            backend.default_model.clone()
        };
        let model = model.map(str::to_owned).unwrap_or_else(|| default);
        *self.active_lock() = Active {
            id: id.to_owned(),
            model,
        };
        Ok(())
    }

    /// Every constructible backend as listing rows, ascending by id.
    pub fn list(&self) -> Vec<BackendRow> {
        self.backends_lock()
            .values()
            .map(|backend| BackendRow {
                id: backend.id.clone(),
                kind: backend.kind.clone(),
                default_model: backend.default_model.clone(),
            })
            .collect()
    }

    /// The live model listing of the instance `id`, when the backend
    /// carries one.
    pub fn list_models(&self, id: &str) -> Option<ModelListing> {
        let backend = self.backends_lock().get(id)?.clone();
        backend.list_models.as_ref().map(|list| list())
    }

    fn active_lock(&self) -> RwLockWriteGuard<'_, Active> {
        self.active.write().unwrap_or_else(PoisonError::into_inner)
    }

    fn backends_lock(&self) -> RwLockWriteGuard<'_, BTreeMap<String, Arc<Backend>>> {
        self.backends
            .write()
            .unwrap_or_else(PoisonError::into_inner)
    }
}

/// The credential-backed seed for the active pair when the config
/// names no provider: the first discovered backend that resolved an
/// actual credential — a stored row, an env key, or the stored rhema
/// profile. The keyless llama.cpp fallback is skipped: it exists so
/// the registry is never empty, not because anyone chose it, and with
/// nothing keyed the registry serves it while the UI's add-a-provider
/// state stays the honest answer. The model stays unset — the
/// registry fills each kind's default.
pub fn credential_seed(backends: &[Backend]) -> Option<(String, Option<String>)> {
    backends
        .iter()
        .find(|backend| backend.kind != "llama-cpp")
        .map(|backend| (backend.id.clone(), None))
}

/// The concrete providers behind [`Backend`]: an enum because the live
/// model listings are inherent methods, and the registry stores the
/// provider behind the object-safe seam. Shared by `Arc`, never cloned.
pub enum AnyProvider {
    /// The Gemini adapter.
    Google(GoogleProvider),
    /// The llama.cpp server adapter.
    LlamaCpp(LlamaCppProvider),
    /// The OpenRouter adapter.
    OpenRouter(OpenRouterProvider),
    /// The Anthropic Messages adapter.
    Anthropic(AnthropicProvider),
    /// The generic OpenAI-compatible adapter.
    OpenAiCompatible(OpenAiCompatibleProvider),
    /// DeepSeek through the OpenAI-compatible adapter at its own base.
    DeepSeek(OpenAiCompatibleProvider),
    /// GitHub Copilot through the Copilot adapter.
    Copilot(CopilotProvider),
}

impl AnyProvider {
    /// The kind's live model listing. Llama servers list their own
    /// `LlamaModel` rows, which carry ids only: mapped into the shared
    /// card shape with nothing but the id filled.
    pub async fn list_models(&self) -> Result<Vec<ModelCard>, ProviderError> {
        match self {
            AnyProvider::Google(provider) => provider.list_models().await,
            AnyProvider::Anthropic(provider) => provider.list_models().await,
            AnyProvider::OpenRouter(provider) => provider.list_models().await,
            AnyProvider::OpenAiCompatible(provider) => provider.list_models().await,
            AnyProvider::DeepSeek(provider) => provider.list_models().await,
            AnyProvider::Copilot(provider) => provider.list_models().await,
            AnyProvider::LlamaCpp(provider) => {
                let models = provider.list_models().await?;
                Ok(models
                    .into_iter()
                    .map(|model| ModelCard {
                        display_name: model.id.clone(),
                        id: model.id,
                        description: String::new(),
                        input_token_limit: model.context_length,
                        output_token_limit: None,
                        generation_methods: vec!["generateContent".to_owned()],
                        pricing: None,
                    })
                    .collect())
            }
        }
    }

    /// The boxed listing a [`Backend`] carries.
    pub fn boxed_listing(provider: Arc<Self>) -> Arc<dyn Fn() -> ModelListing + Send + Sync> {
        Arc::new(move || {
            let provider = provider.clone();
            Box::pin(async move { provider.list_models().await })
        })
    }
}

#[async_trait]
impl ModelProvider for AnyProvider {
    async fn complete(
        &self,
        request: ModelRequest,
        sink: &mut dyn EventSink,
    ) -> Result<ModelOutput, ProviderError> {
        match self {
            AnyProvider::Google(provider) => provider.complete(request, sink).await,
            AnyProvider::LlamaCpp(provider) => provider.complete(request, sink).await,
            AnyProvider::OpenRouter(provider) => provider.complete(request, sink).await,
            AnyProvider::Anthropic(provider) => provider.complete(request, sink).await,
            AnyProvider::OpenAiCompatible(provider) => provider.complete(request, sink).await,
            AnyProvider::DeepSeek(provider) => provider.complete(request, sink).await,
            AnyProvider::Copilot(provider) => provider.complete(request, sink).await,
        }
    }
}

/// The env variable each keyed backend reads, in precedence order
/// (checked before the stored rhema config's value, mirroring the
/// frontend's env-wins rule).
/// The env variable names a kind's credential reads, in precedence
/// order — the list route's `source` computation needs the names, the
/// discovery the values.
pub fn env_key_names(kind: &str) -> &'static [&'static str] {
    env_vars_for(kind)
}

fn env_vars_for(kind: &str) -> &'static [&'static str] {
    match kind {
        "google" => &["GOOGLE_API_KEY", "GEMINI_API_KEY"],
        "anthropic" => &["ANTHROPIC_API_KEY"],
        "openai" => &["OPENAI_API_KEY"],
        "openrouter" => &["OPENROUTER_API_KEY"],
        "deepseek" => &["DEEPSEEK_API_KEY"],
        "copilot" => &["GITHUB_COPILOT_TOKEN"],
        _ => &[],
    }
}

/// DeepSeek speaks the OpenAI-compatible dialect at its own base.
const DEEPSEEK_BASE_URL: &str = "https://api.deepseek.com/v1";

/// The settings-store key prefix under which a backend instance's
/// credentials persist: `provider.<id>` holds a [`ProviderParams`]
/// JSON. Legacy rows stored `provider.<kind>` — the id doubling as the
/// kind, which is what ids default to.
pub const PROVIDER_KEY_PREFIX: &str = "provider.";

/// The settings key for one backend instance's stored credentials.
pub fn provider_settings_key(id: &str) -> String {
    format!("{PROVIDER_KEY_PREFIX}{id}")
}

/// Whether `id` may name a backend instance: trimmed already by the
/// caller, non-empty, no whitespace, no `/`, at most 64 chars.
/// Comparisons are exact.
pub fn valid_instance_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && !id.chars().any(|c| c.is_whitespace() || c == '/')
}

/// The credentials a backend instance constructs from, as the settings
/// store persists them: the kind the params construct, an API key, and
/// for server-addressed kinds the URL. `api_key` never logs and never
/// round-trips through the list route — the list says only where a
/// credential came from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct ProviderParams {
    /// The kind these params construct.
    pub kind: String,
    /// The API key (or GitHub token for copilot).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    /// The server address (llama.cpp).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

/// Every kind a runtime login can construct, ascending: the addable
/// list the HTTP surface serves and the UI renders forms from.
pub fn addable_kinds() -> &'static [&'static str] {
    &[
        "anthropic",
        "copilot",
        "deepseek",
        "google",
        "llama-cpp",
        "openai",
        "openrouter",
    ]
}

/// One field of a kind's login form: the name the credentials body
/// spells it with, and whether the daemon refuses the login without it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct ParamField {
    /// The field name as the credentials route reads it.
    pub name: &'static str,
    /// Whether constructing without the field fails.
    pub required: bool,
}

/// The parameter fields a kind's login takes, in form order — the wire
/// shape under `addable` the UI builds its inputs from. `required`
/// states what the daemon demands, not what the form shows: llama.cpp
/// serves keyless on a default url, so neither of its fields gates the
/// login; the six keyed kinds cannot construct without an `api_key`.
pub fn param_fields_for(kind: &str) -> &'static [ParamField] {
    match kind {
        "llama-cpp" => &[
            ParamField {
                name: "url",
                required: false,
            },
            ParamField {
                name: "api_key",
                required: false,
            },
        ],
        _ => &[ParamField {
            name: "api_key",
            required: true,
        }],
    }
}

/// Constructs one backend from `params`, its id defaulting to the
/// kind. The per-kind requirements: keyed kinds want an `api_key`;
/// llama.cpp wants a `url` (the keyless default still applies when
/// neither is given). Unknown kinds are an error — "mock" is
/// registry-injected, never constructed.
pub fn construct(kind: &str, params: &ProviderParams) -> Result<Backend, String> {
    let provider = match kind {
        "google" => AnyProvider::Google(GoogleProvider::new(require_key(kind, params)?)),
        "anthropic" => AnyProvider::Anthropic(AnthropicProvider::new(require_key(kind, params)?)),
        "openai" => {
            AnyProvider::OpenAiCompatible(OpenAiCompatibleProvider::new(require_key(kind, params)?))
        }
        "openrouter" => {
            AnyProvider::OpenRouter(OpenRouterProvider::new(require_key(kind, params)?))
        }
        "deepseek" => AnyProvider::DeepSeek(
            OpenAiCompatibleProvider::new(require_key(kind, params)?)
                .with_base_url(DEEPSEEK_BASE_URL),
        ),
        "copilot" => AnyProvider::Copilot(CopilotProvider::new(require_key(kind, params)?)),
        "llama-cpp" => {
            let mut provider = LlamaCppProvider::new(
                params
                    .url
                    .clone()
                    .unwrap_or_else(|| rhema_config::Profile::default().llama_cpp_url()),
            );
            if let Some(key) = &params.api_key {
                provider = provider.with_api_key(key);
            }
            AnyProvider::LlamaCpp(provider)
        }
        _ => return Err(format!("unknown provider kind {kind:?}")),
    };
    let default_model = default_model_for(kind).to_owned();
    let provider = Arc::new(provider);
    Ok(Backend {
        id: kind.to_owned(),
        kind: kind.to_owned(),
        provider: provider.clone(),
        default_model,
        list_models: Some(AnyProvider::boxed_listing(provider)),
    })
}

/// The kind's API key, or the error the credentials route answers with.
fn require_key(kind: &str, params: &ProviderParams) -> Result<String, String> {
    params
        .api_key
        .clone()
        .filter(|key| !key.trim().is_empty())
        .ok_or_else(|| format!("{kind} needs an api_key"))
}

/// Discovers every constructible backend instance from the claw's own
/// stored credentials, the environment, and the stored rhema config.
/// Stored instances — the `provider.<id>` rows the HTTP surface wrote —
/// boot under the id they were saved with, the row's inner `kind`
/// naming what to construct (a legacy row without one keeps its id
/// doubling as the kind). For each kind, the env credential outranks
/// everything: it constructs the kind's own id, pushing a stored twin
/// of the same id aside while the row stays persisted for a
/// post-restart return. A stored instance of a kind outranks the
/// profile fallback, which only constructs when neither env nor a
/// stored row resolved. The keyless llama.cpp backend always
/// constructs while no stored row of its kind exists, so the registry
/// is never empty. The config's `provider`/`model` pair is the
/// caller's seed for the active backend — deployment defaults, not a
/// constraint. `stored_rows` reads the daemon's settings store, every
/// `provider.*` row as `(id, params)`; a row that no longer constructs
/// never blocks the boot.
pub fn discover(
    config: &crate::config::ClawConfig,
    stored: &rhema_config::Profile,
    env: &dyn Fn(&str) -> Option<String>,
    stored_rows: &dyn Fn() -> Vec<(String, ProviderParams)>,
    secrets: Option<&dyn rhema_config::secrets::SecretStore>,
) -> Vec<Backend> {
    let rows = stored_rows();
    let mut backends = Vec::new();

    // The env credential each kind resolves from, if any.
    let env_key = |kind: &str| {
        env_vars_for(kind)
            .iter()
            .find_map(|name| env(name))
            .filter(|key| !key.trim().is_empty())
    };

    // Stored instances first, under the id their row names. An
    // env-backed kind takes the id back from a stored twin of the same
    // id below — env wins the per-kind slot, the row stays persisted.
    for (id, params) in &rows {
        if id == &params.kind && env_key(&params.kind).is_some() {
            continue;
        }
        if let Ok(mut backend) = construct(&params.kind, params) {
            backend.id = id.clone();
            seed_llama_model(&mut backend, config);
            backends.push(backend);
        }
    }

    for kind in [
        "llama-cpp",
        "google",
        "anthropic",
        "openai",
        "openrouter",
        "deepseek",
        "copilot",
    ] {
        let stored_of_kind = rows.iter().any(|(_, params)| params.kind == kind);
        let params = if kind == "llama-cpp" {
            if stored_of_kind {
                // The stored rows own the kind now; the keyless default
                // would undo a removal of the kind's own id.
                continue;
            }
            // The keyless server backend always constructs, so the
            // registry is never empty.
            ProviderParams {
                kind: kind.to_owned(),
                api_key: None,
                url: Some(stored.llama_cpp_url()),
            }
        } else if let Some(api_key) = env_key(kind) {
            ProviderParams {
                kind: kind.to_owned(),
                api_key: Some(api_key),
                url: None,
            }
        } else if stored_of_kind {
            // The stored instance of the kind wins over the profile
            // fallback — the surface's login is the newer decision.
            continue;
        } else {
            // The stored rhema profile's key for the kind: the matching
            // named instance's. The keyless llama.cpp backend never
            // takes a key from the profile.
            let api_key = match kind {
                "llama-cpp" => None,
                _ => stored.stored_provider_key(kind, secrets),
            };
            ProviderParams {
                kind: kind.to_owned(),
                api_key,
                url: None,
            }
        };
        let has_creds = match kind {
            // The keyless server backend always constructs.
            "llama-cpp" => true,
            _ => params.api_key.is_some(),
        };
        if !has_creds {
            continue;
        }
        match construct(kind, &params) {
            Ok(mut backend) => {
                seed_llama_model(&mut backend, config);
                backends.push(backend);
            }
            // An unconstructible kind (a blank key) never blocks the boot.
            Err(_) => continue,
        }
    }
    backends
}

/// The config file's model seeds llama's fallback, as discovery always
/// has: it names the deployment's local model when it names one at all.
fn seed_llama_model(backend: &mut Backend, config: &crate::config::ClawConfig) {
    if backend.kind == "llama-cpp" {
        if let Some(model) = &config.model {
            backend.default_model = model.clone();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rhema_config::secrets::SecretStore as _;
    use rhema_core::{Message, Role, StopReason, Usage};

    /// A minimal scripted stand-in: registry tests never complete a turn.
    struct Stub;

    #[async_trait]
    impl ModelProvider for Stub {
        async fn complete(
            &self,
            _req: ModelRequest,
            _sink: &mut dyn EventSink,
        ) -> Result<ModelOutput, ProviderError> {
            Ok(ModelOutput {
                message: Message::text(Role::Assistant, "stub"),
                stop_reason: StopReason::EndTurn,
                usage: Usage::default(),
            })
        }
    }

    fn backend(kind: &str) -> Backend {
        // The real per-kind default, not headless's mock placeholder —
        // the fallback assertions depend on it.
        Backend {
            id: kind.to_owned(),
            kind: kind.to_owned(),
            provider: Arc::new(Stub),
            default_model: default_model_for(kind).to_owned(),
            list_models: None,
        }
    }

    #[test]
    fn the_credential_seed_is_the_first_keyed_backend_in_discovery_order() {
        // The keyless llama.cpp fallback never seeds: it exists so the
        // registry is never empty, not because anyone chose it.
        assert_eq!(
            credential_seed(&[backend("llama-cpp")]),
            None,
            "nothing keyed: no seed, the keyless server serves"
        );
        // The first keyed backend wins, in discovery order.
        assert_eq!(
            credential_seed(&[
                backend("llama-cpp"),
                backend("google"),
                backend("anthropic")
            ]),
            Some(("google".to_owned(), None))
        );
        // A stored named row comes first in discovery order, whatever
        // its kind sorts to in the registry.
        let mut stored = backend("anthropic");
        stored.id = "work-key".to_owned();
        assert_eq!(
            credential_seed(&[stored, backend("google")]),
            Some(("work-key".to_owned(), None))
        );
        // The model stays unset: the registry fills the kind default.
        let (id, model) = credential_seed(&[backend("openai")]).unwrap();
        assert_eq!((id.as_str(), model), ("openai", None));
    }

    #[test]
    fn the_seed_wins_when_the_kind_exists() {
        let registry = BackendRegistry::new(
            vec![backend("anthropic"), backend("google")],
            Some(("google".to_owned(), Some("gemini-3.8-flash".to_owned()))),
        )
        .unwrap();
        assert_eq!(
            registry.active(),
            ("google".to_owned(), "gemini-3.8-flash".to_owned())
        );
    }

    #[test]
    fn a_seed_for_an_absent_kind_falls_back_to_the_first_backend() {
        let registry = BackendRegistry::new(
            vec![backend("anthropic"), backend("google")],
            Some(("openai".to_owned(), None)),
        )
        .unwrap();
        assert_eq!(registry.active().0, "anthropic", "ascending first");
    }

    #[test]
    fn a_switch_falls_back_to_the_kind_default_without_a_model() {
        let registry =
            BackendRegistry::new(vec![backend("anthropic"), backend("google")], None).unwrap();
        registry.set_active("google", None).unwrap();
        assert_eq!(registry.active().1, "gemini-3.8-flash");
        registry
            .set_active("anthropic", Some("claude-opus-4-1"))
            .unwrap();
        assert_eq!(registry.active().1, "claude-opus-4-1");
    }

    #[test]
    fn an_unknown_kind_is_rejected() {
        let registry = BackendRegistry::new(vec![backend("google")], None).unwrap();
        assert!(registry.set_active("openai", None).is_err());
    }

    #[test]
    fn an_empty_registry_is_a_construction_error() {
        assert!(BackendRegistry::new(vec![], None).is_err());
    }

    fn params(kind: &str, api_key: Option<&str>, url: Option<&str>) -> ProviderParams {
        ProviderParams {
            kind: kind.to_owned(),
            api_key: api_key.map(str::to_owned),
            url: url.map(str::to_owned),
        }
    }

    #[test]
    fn instance_ids_exclude_whitespace_slashes_and_excess_length() {
        assert!(valid_instance_id("local-llama"));
        assert!(valid_instance_id(&"x".repeat(64)));
        assert!(!valid_instance_id(""));
        assert!(!valid_instance_id("a b"));
        assert!(!valid_instance_id("a/b"));
        assert!(!valid_instance_id(&"x".repeat(65)));
    }

    #[test]
    fn two_instances_of_a_kind_coexist_under_their_ids() {
        let mut local_a = backend("llama-cpp");
        local_a.id = "local-a".to_owned();
        let mut local_b = backend("llama-cpp");
        local_b.id = "local-b".to_owned();
        let registry = BackendRegistry::new(vec![local_a, local_b], None).unwrap();
        assert_eq!(registry.list().len(), 2);
        registry.set_active("local-b", None).unwrap();
        assert_eq!(registry.active().0, "local-b");
        // Removing the active id hands the pair to the remaining one.
        registry.remove("local-b").unwrap();
        assert_eq!(registry.active().0, "local-a");
    }

    #[test]
    fn construct_demands_what_each_kind_needs() {
        // Keyed kinds want a non-blank key.
        assert!(construct("google", &params("google", None, None)).is_err());
        assert!(construct("google", &params("google", Some("  "), None)).is_err());
        assert!(construct("google", &params("google", Some("k"), None)).is_ok());
        assert!(construct("deepseek", &params("deepseek", Some("k"), None)).is_ok());
        assert!(construct("copilot", &params("copilot", Some("t"), None)).is_ok());
        assert!(construct("nope", &params("nope", Some("k"), None)).is_err());
        // llama.cpp serves keyless on its default url.
        assert!(construct("llama-cpp", &params("llama-cpp", None, None)).is_ok());
        assert!(construct("llama-cpp", &params("llama-cpp", None, Some("http://h:1"))).is_ok());
    }

    #[test]
    fn discover_builds_exactly_the_kinds_with_a_source() {
        let config = test_config();
        let profile = profile_with_google();
        let env = |_name: &str| None;
        let stored_rows = || vec![("deepseek".to_owned(), params("deepseek", Some("cd"), None))];
        let backends = discover(&config, &profile, &env, &stored_rows, None);
        let mut kinds: Vec<&str> = backends.iter().map(|b| b.kind.as_str()).collect();
        kinds.sort_unstable();
        assert_eq!(kinds, vec!["deepseek", "google", "llama-cpp"]);
    }

    #[test]
    fn discover_boots_llama_even_when_nobody_stored_anything() {
        let config = test_config();
        let profile = rhema_config::Profile::default();
        let env = |_name: &str| None;
        let backends = discover(&config, &profile, &env, &|| Vec::new(), None);
        assert_eq!(backends.len(), 1);
        assert_eq!(backends[0].kind, "llama-cpp");
    }

    #[test]
    fn a_keychain_ref_serves_the_profile_fallback_without_a_literal() {
        let config = test_config();
        // The stored instance logs in through the chain: no literal, a
        // live ref. Discovery resolves it through the store tier.
        let mut profile = rhema_config::Profile::default();
        profile.upsert_instance(
            "google",
            rhema_config::ProviderInstance {
                provider: "google".into(),
                api_key: None,
                secret_ref: Some("ref-1".into()),
                base_url: None,
            },
        );
        let store = rhema_config::secrets::VolatileSecretStore::new();
        store.set("ref-1", "chain-key").unwrap();
        let env = |_name: &str| None;
        let backends = discover(&config, &profile, &env, &|| Vec::new(), Some(&store));
        assert!(
            backends.iter().any(|b| b.kind == "google"),
            "the keychain key builds the backend"
        );
        // Without the store the ref is unreadable and the kind is not
        // constructible — the honest logged-out shape, not a guess.
        let backends = discover(&config, &profile, &env, &|| Vec::new(), None);
        assert!(!backends.iter().any(|b| b.kind == "google"));
    }

    #[test]
    fn a_named_row_boots_under_its_id_and_keeps_the_default_slot_empty() {
        let config = test_config();
        let profile = profile_with_google();
        let env = |_name: &str| None;
        let stored_rows = || {
            vec![(
                "local-llama".to_owned(),
                params("llama-cpp", None, Some("http://h:1")),
            )]
        };
        let backends = discover(&config, &profile, &env, &stored_rows, None);
        let mut ids: Vec<&str> = backends.iter().map(|b| b.id.as_str()).collect();
        ids.sort_unstable();
        assert_eq!(ids, vec!["google", "local-llama"]);
        // The stored row owns the kind: the keyless default id does not
        // resurrect beside it.
        assert!(!ids.contains(&"llama-cpp"));
        let llama = backends.iter().find(|b| b.id == "local-llama").unwrap();
        assert_eq!(llama.kind, "llama-cpp");
    }

    #[test]
    fn env_takes_the_kind_id_back_from_a_stored_twin() {
        let config = test_config();
        let profile = rhema_config::Profile::default();
        let env = |name: &str| (name == "GOOGLE_API_KEY").then(|| "from-env".to_owned());
        let stored_rows = || vec![("google".to_owned(), params("google", Some("stored"), None))];
        let backends = discover(&config, &profile, &env, &stored_rows, None);
        let google: Vec<&Backend> = backends.iter().filter(|b| b.kind == "google").collect();
        assert_eq!(google.len(), 1, "env wins the id, no twin survives");
        assert_eq!(google[0].id, "google");
    }

    #[test]
    fn a_row_stored_under_its_kind_id_boots_as_its_kind() {
        let config = test_config();
        let profile = rhema_config::Profile::default();
        let env = |_name: &str| None;
        let stored_rows = || vec![("anthropic".to_owned(), params("anthropic", Some("k"), None))];
        let backends = discover(&config, &profile, &env, &stored_rows, None);
        let anthropic = backends
            .iter()
            .find(|b| b.id == "anthropic")
            .expect("the stored row boots");
        assert_eq!(anthropic.kind, "anthropic");
        assert_eq!(anthropic.default_model, "claude-sonnet-4-5");
    }

    #[test]
    fn insert_replaces_and_remove_moves_the_active_pair() {
        let registry = BackendRegistry::new(
            vec![backend("anthropic"), backend("google")],
            Some(("google".to_owned(), None)),
        )
        .unwrap();

        // A replace: the kind count holds, the default may change.
        registry.insert(backend("google"));
        assert_eq!(registry.list().len(), 2);

        // An addition joins the list.
        registry.insert(backend("openai"));
        assert_eq!(registry.list().len(), 3);

        // Removing the active kind hands the pair to the first
        // remaining backend, ascending.
        registry.remove("google").unwrap();
        assert_eq!(registry.active().0, "anthropic");

        // The last backend refuses to go.
        registry.remove("openai").unwrap();
        assert!(registry.remove("anthropic").is_err());
        assert_eq!(registry.list().len(), 1);
    }

    fn test_config() -> crate::config::ClawConfig {
        serde_json::from_value(serde_json::json!({
            "poll_interval_secs": 60,
            "watches": []
        }))
        .expect("a minimal claw config")
    }

    fn profile_with_google() -> rhema_config::Profile {
        let mut profile = rhema_config::Profile::default();
        profile.upsert_instance(
            "google",
            rhema_config::ProviderInstance {
                provider: "google".into(),
                api_key: Some("pg".into()),
                secret_ref: None,
                base_url: None,
            },
        );
        profile
    }
}
