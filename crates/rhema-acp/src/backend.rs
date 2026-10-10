//! Backend resolution: which provider and model serve ACP turns.
//!
//! The same startup resolution the headless one-shot performs — a
//! provider choice (flag, then stored config, then google), a model
//! (flag, then stored, then the kind's fallback), and whatever
//! credentials the profile holds. `None` means no credential resolved:
//! to the client that is `auth_required`, the one honest answer.

use std::sync::Arc;

use rhema_config::Profile;
use rhema_core::ModelProvider;
use rhema_provider::ModelCard;
use rhema_provider_anthropic::AnthropicProvider;
use rhema_provider_google::GoogleProvider;
use rhema_provider_llama_cpp::LlamaCppProvider;
use rhema_provider_openai_compatible::OpenAiCompatibleProvider;
use rhema_provider_openrouter::OpenRouterProvider;

/// A backend that can serve turns: its provider and the model to run.
pub struct Resolved {
    /// The provider implementation behind the agent.
    pub provider: Arc<dyn ModelProvider>,
    /// The model every turn runs on.
    pub model: String,
}

/// The per-kind model fallback when neither the flag nor the config
/// names one — the same defaults the headless one-shot uses.
pub fn default_model_for(provider: &str) -> &'static str {
    match provider {
        "llama-cpp" => "local",
        "anthropic" => "claude-sonnet-4-5",
        "openai" => "gpt-5",
        "deepseek" => "deepseek-chat",
        // Google and OpenRouter share the Gemini fallback; OpenRouter
        // routes it.
        _ => "gemini-3.8-flash",
    }
}

/// The catalog rows of the provider the profile (or an override) points
/// at: the active provider's text-chat models, fetched over its API.
/// `provider` and `llama_url` apply the same overrides as [`resolve`];
/// `None` when that kind has no resolvable credentials or the listing
/// fails. These are the choices a client's model picker renders.
pub async fn catalog(
    profile: &Profile,
    provider: Option<&str>,
    llama_url: Option<&str>,
) -> Option<Vec<ModelCard>> {
    let kind = provider.or(profile.provider.as_deref()).unwrap_or("google");
    let models = match kind {
        "llama-cpp" => {
            let url = llama_url
                .map(str::to_owned)
                .unwrap_or_else(|| profile.llama_cpp_url());
            let mut llama = LlamaCppProvider::new(url);
            if let Some(api_key) = profile.llama_cpp_api_key() {
                llama = llama.with_api_key(api_key);
            }
            llama
                .list_models()
                .await
                .ok()?
                .into_iter()
                .map(|m| ModelCard {
                    id: m.id.clone(),
                    display_name: m.id.clone(),
                    description: String::new(),
                    input_token_limit: m.context_length,
                    output_token_limit: None,
                    generation_methods: vec!["generateContent".to_owned()],
                    pricing: None,
                })
                .collect()
        }
        "openrouter" => {
            let key = profile.openrouter_api_key()?;
            OpenRouterProvider::new(key).list_models().await.ok()?
        }
        "anthropic" => {
            let key = profile.anthropic_api_key()?;
            AnthropicProvider::new(key).list_models().await.ok()?
        }
        "openai" => {
            let key = profile.openai_api_key()?;
            OpenAiCompatibleProvider::new(key)
                .list_models()
                .await
                .ok()?
        }
        "deepseek" => {
            let key = profile.deepseek_api_key()?;
            OpenAiCompatibleProvider::new(key)
                .with_base_url("https://api.deepseek.com/v1")
                .list_models()
                .await
                .ok()?
        }
        _ => {
            let key = profile.google_api_key()?;
            GoogleProvider::new(key).list_models().await.ok()?
        }
    };
    Some(models)
}

/// Resolves the backend the profile's settings call for. `provider`
/// overrides the stored choice; `model` the stored model; `llama_url`
/// the stored server URL. `None` when the chosen kind has no usable
/// credentials (or an explicit choice names a kind that does not
/// resolve).
pub fn resolve(
    profile: &Profile,
    provider: Option<&str>,
    model: Option<&str>,
    llama_url: Option<&str>,
) -> Option<Resolved> {
    let kind = provider.or(profile.provider.as_deref()).unwrap_or("google");
    let model = model
        .map(str::to_owned)
        .or_else(|| profile.model.clone())
        .unwrap_or_else(|| default_model_for(kind).to_owned());
    let provider: Arc<dyn ModelProvider> = match kind {
        "llama-cpp" => {
            let url = llama_url
                .map(str::to_owned)
                .unwrap_or_else(|| profile.llama_cpp_url());
            let mut llama = LlamaCppProvider::new(url);
            if let Some(api_key) = profile.llama_cpp_api_key() {
                llama = llama.with_api_key(api_key);
            }
            Arc::new(llama)
        }
        "openrouter" => Arc::new(OpenRouterProvider::new(profile.openrouter_api_key()?)),
        "anthropic" => Arc::new(AnthropicProvider::new(profile.anthropic_api_key()?)),
        "openai" => Arc::new(OpenAiCompatibleProvider::new(profile.openai_api_key()?)),
        "deepseek" => Arc::new(
            OpenAiCompatibleProvider::new(profile.deepseek_api_key()?)
                .with_base_url("https://api.deepseek.com/v1"),
        ),
        _ => Arc::new(GoogleProvider::new(profile.google_api_key()?)),
    };
    Some(Resolved { provider, model })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(provider: Option<&str>, model: Option<&str>) -> Profile {
        Profile {
            provider: provider.map(str::to_owned),
            model: model.map(str::to_owned),
            ..Profile::default()
        }
    }

    #[test]
    fn the_keyless_backend_always_resolves() {
        let resolved = resolve(&profile(Some("llama-cpp"), None), None, None, None)
            .expect("llama-cpp is keyless and always resolves");
        assert_eq!(resolved.model, "local");
    }

    #[test]
    fn the_flags_override_the_stored_choices() {
        let resolved = resolve(
            &profile(Some("openrouter"), Some("custom-model")),
            Some("llama-cpp"),
            Some("flag-model"),
            Some("http://localhost:9999"),
        )
        .expect("llama-cpp is keyless and always resolves");
        assert_eq!(resolved.model, "flag-model");
    }

    #[test]
    fn the_model_falls_back_config_then_kind_default() {
        let stored = resolve(
            &profile(Some("llama-cpp"), Some("stored-model")),
            None,
            None,
            None,
        )
        .expect("llama-cpp is keyless and always resolves");
        assert_eq!(stored.model, "stored-model");
    }

    #[test]
    fn an_explicit_choice_to_a_keyed_kind_can_only_fail() {
        // Without a resolvable key an explicit openrouter choice is
        // `None` — the serve layer answers `auth_required`. A key in the
        // sandbox env would flip this; CI keeps the env bare.
        if std::env::var_os("OPENROUTER_API_KEY").is_some() {
            return;
        }
        assert!(resolve(&profile(None, None), Some("openrouter"), None, None).is_none());
    }
}
