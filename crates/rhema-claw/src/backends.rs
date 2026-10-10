//! The backend registry: every provider the deployment can speak to, one
//! active at a time, switchable at runtime.
//!
//! At boot the registry constructs every provider whose credentials
//! resolve — env vars win, stored keys serve, and the keyless
//! llama-cpp backend is always constructible, so a daemon can always
//! boot. One backend is *active*; handlers read the registry per
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
use rhema_provider_google::GoogleProvider;
use rhema_provider_llama_cpp::LlamaCppProvider;
use rhema_provider_openai_compatible::OpenAiCompatibleProvider;
use rhema_provider_openrouter::OpenRouterProvider;

use rhema_provider::ModelCard;

/// The default model per backend kind, when neither the operator nor the
/// settings store names one.
pub fn default_model_for(kind: &str) -> &'static str {
    match kind {
        "llama-cpp" => "local",
        "anthropic" => "claude-sonnet-4-5",
        "openai" => "gpt-5",
        "openrouter" => "openrouter/auto",
        _ => "gemini-3.8-flash",
    }
}

/// The listing of a card-backed backend, boxed for the registry; a
/// backend without one (the mock, llama's raw id list) carries `None`.
pub type ModelListing = Pin<Box<dyn Future<Output = Result<Vec<ModelCard>, ProviderError>> + Send>>;

/// One constructible backend: the provider seam, its kind, its model
/// fallback, and how to list its models.
pub struct Backend {
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
    /// A backend without a live listing (the mock).
    pub fn headless(kind: impl Into<String>, provider: Arc<dyn ModelProvider>) -> Self {
        Self {
            kind: kind.into(),
            provider,
            default_model: "mock".to_owned(),
            list_models: None,
        }
    }
}

/// The currently active backend: kind plus the model turns run on.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Active {
    kind: String,
    model: String,
}

/// The deployment's providers and the active pair, shared by every
/// handler so a switch takes effect on the next dispatch.
pub struct BackendRegistry {
    backends: BTreeMap<String, Arc<Backend>>,
    active: RwLock<Active>,
}

impl BackendRegistry {
    /// A registry over `backends`. The active pair is `seed` when that
    /// kind exists, else the first backend's kind and default model.
    /// An empty list is a construction error: the registry exists so
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
            .map(|backend| (backend.kind.clone(), Arc::new(backend)))
            .collect();
        let active = match seed {
            Some((kind, model)) if map.contains_key(&kind) => {
                let default = map[&kind].default_model.clone();
                Active {
                    kind,
                    model: model.unwrap_or(default),
                }
            }
            _ => {
                let first = map.values().next().expect("non-empty above");
                Active {
                    kind: first.kind.clone(),
                    model: first.default_model.clone(),
                }
            }
        };
        Ok(Self {
            backends: map,
            active: RwLock::new(active),
        })
    }

    /// The provider and model the next dispatch runs on.
    pub fn current(&self) -> (Arc<dyn ModelProvider>, String) {
        let active = self.active_lock();
        let backend = &self.backends[&active.kind];
        (backend.provider.clone(), active.model.clone())
    }

    /// The active pair as data: kind and model.
    pub fn active(&self) -> (String, String) {
        let active = self.active_lock();
        (active.kind.clone(), active.model.clone())
    }

    /// Switches the active pair. An unknown kind is an error; a model
    /// left unset falls back to that kind's default.
    pub fn set_active(&self, kind: &str, model: Option<&str>) -> Result<(), String> {
        let backend = self
            .backends
            .get(kind)
            .ok_or_else(|| format!("unknown provider kind {kind:?}"))?;
        let model = model
            .map(str::to_owned)
            .unwrap_or_else(|| backend.default_model.clone());
        *self.active_lock() = Active {
            kind: kind.to_owned(),
            model,
        };
        Ok(())
    }

    /// Every constructible backend: kind and model fallback, ascending.
    pub fn list(&self) -> Vec<(String, String)> {
        self.backends
            .values()
            .map(|backend| (backend.kind.clone(), backend.default_model.clone()))
            .collect()
    }

    /// The live model listing of `kind`, when the backend carries one.
    pub fn list_models(&self, kind: &str) -> Option<ModelListing> {
        let backend = self.backends.get(kind)?;
        backend.list_models.as_ref().map(|list| list())
    }

    fn active_lock(&self) -> RwLockWriteGuard<'_, Active> {
        self.active.write().unwrap_or_else(PoisonError::into_inner)
    }
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
        }
    }
}

/// The env variable each keyed backend reads, in precedence order
/// (checked before the stored rhema config's value, mirroring the
/// frontend's env-wins rule).
fn env_vars_for(kind: &str) -> &'static [&'static str] {
    match kind {
        "google" => &["GOOGLE_API_KEY", "GEMINI_API_KEY"],
        "anthropic" => &["ANTHROPIC_API_KEY"],
        "openai" => &["OPENAI_API_KEY"],
        "openrouter" => &["OPENROUTER_API_KEY"],
        _ => &[],
    }
}

/// Discovers every constructible backend from the config seed, the
/// stored rhema credentials, and the environment. The keyless llama.cpp
/// backend is always constructed, so the registry is never empty; a
/// keyed backend joins only when its env var or stored key resolves.
/// The config's `provider`/`model` pair is the caller's seed for the
/// active backend — deployment defaults, not a constraint.
pub fn discover(
    config: &crate::config::ClawConfig,
    stored: &rhema_config::Profile,
    env: &dyn Fn(&str) -> Option<String>,
) -> Vec<Backend> {
    let mut backends = Vec::new();
    let mut add = |kind: &str, provider: AnyProvider, stored_default: Option<String>| {
        let default_model = stored_default.unwrap_or_else(|| default_model_for(kind).to_owned());
        let provider = Arc::new(provider);
        backends.push(Backend {
            kind: kind.to_owned(),
            provider: provider.clone(),
            default_model,
            list_models: Some(AnyProvider::boxed_listing(provider)),
        });
    };

    // llama-cpp first: keyless, so the registry is never empty.
    {
        let mut provider = LlamaCppProvider::new(stored.llama_cpp_url());
        if let Some(key) = &stored.llama_cpp_api_key {
            provider = provider.with_api_key(key);
        }
        add(
            "llama-cpp",
            AnyProvider::LlamaCpp(provider),
            config.model.clone(),
        );
    }
    if stored.google_api_key.is_some()
        || env_vars_for("google")
            .iter()
            .any(|name| env(name).is_some())
    {
        add(
            "google",
            AnyProvider::Google(GoogleProvider::new(
                env_vars_for("google")
                    .iter()
                    .find_map(|name| env(name))
                    .or_else(|| stored.google_api_key.clone())
                    .expect("a key resolved above"),
            )),
            None,
        );
    }
    if stored.anthropic_api_key.is_some() || env("ANTHROPIC_API_KEY").is_some() {
        add(
            "anthropic",
            AnyProvider::Anthropic(AnthropicProvider::new(
                env("ANTHROPIC_API_KEY")
                    .or_else(|| stored.anthropic_api_key.clone())
                    .expect("a key resolved above"),
            )),
            None,
        );
    }
    if stored.openai_api_key.is_some() || env("OPENAI_API_KEY").is_some() {
        add(
            "openai",
            AnyProvider::OpenAiCompatible(OpenAiCompatibleProvider::new(
                env("OPENAI_API_KEY")
                    .or_else(|| stored.openai_api_key.clone())
                    .expect("a key resolved above"),
            )),
            None,
        );
    }
    if stored.openrouter_api_key.is_some() || env("OPENROUTER_API_KEY").is_some() {
        add(
            "openrouter",
            AnyProvider::OpenRouter(OpenRouterProvider::new(
                env("OPENROUTER_API_KEY")
                    .or_else(|| stored.openrouter_api_key.clone())
                    .expect("a key resolved above"),
            )),
            None,
        );
    }
    backends
}

#[cfg(test)]
mod tests {
    use super::*;
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
            kind: kind.to_owned(),
            provider: Arc::new(Stub),
            default_model: default_model_for(kind).to_owned(),
            list_models: None,
        }
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
}
