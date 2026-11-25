//! The models.dev projection and the lookup that turns a provider id
//! into a model's published facts.

use std::collections::BTreeMap;

use rhema_provider::Pricing;
use serde::{Deserialize, Serialize};

/// What models.dev knows about one model id. Every field stays
/// optional: the catalog carries what its maintainers recorded, and a
/// missing field is unknown, not a default.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Facts {
    /// Human-readable name.
    pub display_name: Option<String>,
    /// What the model is for.
    pub description: Option<String>,
    /// Per-MTok price, absent for a subscription-only route.
    pub pricing: Option<Pricing>,
    /// Prompt ceiling in tokens.
    pub input_token_limit: Option<u64>,
    /// Output ceiling in tokens.
    pub output_token_limit: Option<u64>,
    /// Whether the model accepts image input.
    pub supports_vision: Option<bool>,
    /// Whether the model accepts tool calls.
    pub supports_tools: Option<bool>,
    /// Whether the model reasons before it answers.
    pub supports_reasoning: Option<bool>,
    /// The model family (e.g. `claude-sonnet`).
    pub family: Option<String>,
    /// Training cutoff, as models.dev spells it.
    pub knowledge_cutoff: Option<String>,
    /// Release date, `YYYY-MM-DD`.
    pub release_date: Option<String>,
    /// Whether the weights are downloadable.
    pub open_weights: Option<bool>,
    /// The vendor id the row canonicalizes to, when models.dev gives
    /// one (a reseller row names the first-party model).
    pub canonical_model_id: Option<String>,
}

/// The models.dev projection: provider key to the models under it.
/// Parsing keeps only the keys the provider table reads, so the
/// 6.9 MB response shrinks to the rows this workspace can show.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(transparent)]
pub struct Catalog(BTreeMap<String, Provider>);

/// One provider's rows, keyed by model id.
#[derive(Debug, Clone, Deserialize, Serialize)]
struct Provider {
    #[serde(default)]
    models: BTreeMap<String, Row>,
}

/// One stored row, projected from models.dev's model object.
#[derive(Debug, Clone, Deserialize, Serialize)]
struct Row {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    family: Option<String>,
    #[serde(default)]
    cost: Option<Cost>,
    #[serde(default)]
    limit: Option<Limit>,
    #[serde(default)]
    modalities: Option<Modalities>,
    #[serde(default)]
    tool_call: Option<bool>,
    #[serde(default)]
    reasoning: Option<bool>,
    #[serde(default)]
    knowledge: Option<String>,
    #[serde(default)]
    release_date: Option<String>,
    #[serde(default)]
    open_weights: Option<bool>,
    #[serde(default)]
    canonical_model_id: Option<String>,
}

/// The per-MTok price block. models.dev also publishes `cache_write`
/// and `tiers`; the card's [`Pricing`] has no field for either, so the
/// projection drops them and the base tier stands.
#[derive(Debug, Clone, Deserialize, Serialize)]
struct Cost {
    #[serde(default)]
    input: Option<f64>,
    #[serde(default)]
    output: Option<f64>,
    #[serde(default)]
    cache_read: Option<f64>,
}

/// The token ceilings. `input` is the prompt ceiling when the
/// provider publishes a tighter one than the context window (OpenAI
/// does for the Codex rows); otherwise `context` is the ceiling.
#[derive(Debug, Clone, Deserialize, Serialize)]
struct Limit {
    #[serde(default)]
    context: Option<u64>,
    #[serde(default)]
    input: Option<u64>,
    #[serde(default)]
    output: Option<u64>,
}

/// The input modalities, the one place vision is recorded.
#[derive(Debug, Clone, Deserialize, Serialize)]
struct Modalities {
    #[serde(default)]
    input: Option<Vec<String>>,
}

/// One config provider spelling: the models.dev key it reads, a second
/// key for a missing row, and whether the route bills per token.
struct Policy {
    /// The models.dev provider key.
    key: &'static str,
    /// A second key to read when the first has no row for an id. It
    /// answers facts only: a row from another route may carry a
    /// resale price, and that is not this route's bill.
    fallback: Option<&'static str>,
    /// Whether the route bills per token. A subscription plan does
    /// not: its listing may still carry the vendor's list price, and
    /// adding that to a session cost would show a bill nobody pays.
    prices: bool,
}

/// The provider table, one row per config spelling. Subscription
/// backends keep the facts and drop the price. llama.cpp serves
/// whatever the local server holds, which no public catalog lists, so
/// it has no row.
const POLICIES: &[(&str, &str, Option<&str>, bool)] = &[
    ("google", "google", None, true),
    ("anthropic", "anthropic", None, true),
    // OpenAI's own key misses the Codex ids the gateway lists.
    ("openai", "openai", Some("opencode"), true),
    // The ChatGPT subscription serves the OpenAI catalog on a flat
    // fee, the same rows with no metered price.
    ("chatgpt", "openai", Some("opencode"), false),
    ("copilot", "github-copilot", None, false),
    ("openrouter", "openrouter", None, true),
    ("opencode", "opencode", None, true),
    ("deepseek", "deepseek", None, true),
    ("zai", "zai", None, true),
    // Vibe's included usage draws at the API's published rates.
    ("mistral", "mistral", None, true),
    ("mistral-vibe", "mistral", None, true),
];

/// The row for a config spelling.
fn policy(provider: &str) -> Option<Policy> {
    POLICIES
        .iter()
        .find(|(name, ..)| *name == provider)
        .map(|(_, key, fallback, prices)| Policy {
            key,
            fallback: *fallback,
            prices: *prices,
        })
}

/// Whether the projection keeps a models.dev provider key.
fn keeps(key: &str) -> bool {
    POLICIES
        .iter()
        .any(|(_, source, fallback, _)| *source == key || *fallback == Some(key))
}

/// The models.dev key a config provider spelling reads, when it has
/// one.
pub fn provider_key(provider: &str) -> Option<&'static str> {
    policy(provider).map(|policy| policy.key)
}

/// Model spellings that name one of Rhema's shipped defaults but not
/// the id models.dev carries. Keep this table to spellings whose
/// referent is not in doubt: a wrong alias prices the wrong model.
const ALIASES: &[(&str, &str, &str)] = &[
    // DeepSeek keeps the moving id as its documented default.
    ("deepseek", "deepseek-chat", "deepseek-v4-flash"),
    // Rhema stores the four-part spelling, models.dev the trimmed one.
    ("mistral", "mistral-large-4-0", "mistral-large-4"),
];

/// The explicit alias for a provider key and id, when the table has
/// one.
fn alias<'a>(provider_key: &str, id: &'a str) -> Option<&'a str> {
    ALIASES
        .iter()
        .find(|(key, from, _)| *key == provider_key && from.eq_ignore_ascii_case(id))
        .map(|(_, _, to)| *to)
}

/// The slug two spellings of one model share: lowercase, dots and
/// underscores as dashes, the vendor prefix dropped, and the moving
/// aliases (`-latest`, `-preview`) removed. A `-free` or batch suffix
/// stays, it names a different row.
fn normalize(id: &str) -> String {
    let lower = id.to_ascii_lowercase();
    let tail = lower.rsplit('/').next().unwrap_or(&lower);
    let mut slug = tail.replace(['.', '_'], "-");
    for suffix in ["-latest", "-preview"] {
        if let Some(stripped) = slug.strip_suffix(suffix) {
            slug = stripped.to_owned();
        }
    }
    slug
}

impl Catalog {
    /// Reads a models.dev `api.json`, keeping the providers the
    /// provider table reads and dropping the rest.
    ///
    /// # Errors
    ///
    /// Fails when the bytes are not a models.dev provider map.
    pub fn parse(json: &str) -> Result<Self, serde_json::Error> {
        let mut providers: BTreeMap<String, Provider> = serde_json::from_str(json)?;
        providers.retain(|key, _| keeps(key));
        Ok(Self(providers))
    }

    /// The published facts for one model, looked up under the
    /// provider's key. `None` for a provider the catalog does not
    /// cover or an id no row answers to.
    pub fn facts(&self, provider: &str, model_id: &str) -> Option<Facts> {
        let policy = policy(provider)?;
        let (row, prices) = match self.row(policy.key, model_id) {
            Some(row) => (row, policy.prices),
            None => {
                let fallback = policy.fallback.and_then(|key| self.row(key, model_id))?;
                // Another route's row answers the facts, never its
                // price.
                (fallback, false)
            }
        };
        Some(row.facts(prices))
    }

    /// Fills each card's blank facts from the catalog, leaving every
    /// value the listing already carries alone. The adapter stays the
    /// authority on which ids exist and on prices it measured itself;
    /// the catalog answers only where the listing left nothing.
    pub fn decorate(&self, provider: &str, models: &mut [rhema_provider::ModelCard]) {
        for card in models {
            let Some(facts) = self.facts(provider, &card.id) else {
                continue;
            };
            if (card.display_name.is_empty() || card.display_name == card.id)
                && let Some(name) = facts.display_name
            {
                card.display_name = name;
            }
            if card.description.is_empty()
                && let Some(description) = facts.description
            {
                card.description = description;
            }
            if card.input_token_limit.is_none() {
                card.input_token_limit = facts.input_token_limit;
            }
            if card.output_token_limit.is_none() {
                card.output_token_limit = facts.output_token_limit;
            }
            if card.supports_vision.is_none() {
                card.supports_vision = facts.supports_vision;
            }
            if card.pricing.is_none() {
                card.pricing = facts.pricing;
            }
        }
    }

    /// The row for an id: exact, case-insensitive, explicit alias,
    /// then the normalized slug.
    fn row(&self, provider_key: &str, model_id: &str) -> Option<&Row> {
        let models = &self.0.get(provider_key)?.models;
        if let Some(row) = models.get(model_id) {
            return Some(row);
        }
        if let Some(row) = models
            .iter()
            .find(|(id, _)| id.eq_ignore_ascii_case(model_id))
            .map(|(_, row)| row)
        {
            return Some(row);
        }
        let lower = model_id.to_ascii_lowercase();
        let target = alias(provider_key, &lower).unwrap_or(lower.as_str());
        let wanted = normalize(target);
        models
            .iter()
            .find(|(id, _)| normalize(id) == wanted)
            .map(|(_, row)| row)
    }

    /// Test-only presence check for the parse filter.
    #[cfg(test)]
    fn has(&self, provider_key: &str) -> bool {
        self.0.contains_key(provider_key)
    }
}

impl Row {
    /// The row's facts, with pricing removed on a route that bills a
    /// flat fee.
    fn facts(&self, prices: bool) -> Facts {
        Facts {
            display_name: self.name.clone(),
            description: self.description.clone(),
            pricing: if prices { self.pricing() } else { None },
            input_token_limit: self
                .limit
                .as_ref()
                .and_then(|limit| limit.input.or(limit.context)),
            output_token_limit: self.limit.as_ref().and_then(|limit| limit.output),
            supports_vision: self.modalities.as_ref().and_then(|modalities| {
                modalities
                    .input
                    .as_ref()
                    .map(|input| input.iter().any(|kind| kind == "image"))
            }),
            supports_tools: self.tool_call,
            supports_reasoning: self.reasoning,
            family: self.family.clone(),
            knowledge_cutoff: self.knowledge.clone(),
            release_date: self.release_date.clone(),
            open_weights: self.open_weights,
            canonical_model_id: self.canonical_model_id.clone(),
        }
    }

    /// The card's price record. Both halves are required: a row with
    /// one side priced cannot answer what a call costs.
    fn pricing(&self) -> Option<Pricing> {
        let cost = self.cost.as_ref()?;
        Some(Pricing {
            input_per_mtok: cost.input?,
            output_per_mtok: cost.output?,
            cached_input_per_mtok: cost.cache_read,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = r#"
    {
      "anthropic": { "models": {
        "claude-sonnet-4-6": {
          "name": "Claude Sonnet 4.6",
          "description": "A workhorse.",
          "family": "claude-sonnet",
          "cost": { "input": 3, "output": 15, "cache_read": 0.3 },
          "limit": { "context": 1000000, "output": 128000 },
          "modalities": { "input": ["text", "image", "pdf"] },
          "tool_call": true,
          "reasoning": true,
          "knowledge": "2025-08-31",
          "release_date": "2026-02-17",
          "open_weights": false
        }
      }},
      "openai": { "models": {
        "gpt-5.1-codex": {
          "name": "GPT-5.1 Codex",
          "cost": { "input": 1.07, "output": 8.5 },
          "limit": { "context": 400000, "input": 272000, "output": 128000 },
          "modalities": { "input": ["text", "image"] }
        }
      }},
      "zai": { "models": {
        "glm-4.5": {
          "name": "GLM-4.5",
          "cost": { "input": 0.6, "output": 2.2, "cache_read": 0.11 },
          "limit": { "context": 131072, "output": 98304 },
          "modalities": { "input": ["text"] }
        }
      }},
      "mistral": { "models": {
        "mistral-large-4": {
          "name": "Mistral Large 4",
          "cost": { "input": 0.7, "output": 2.1 },
          "limit": { "context": 262144 }
        }
      }},
      "deepseek": { "models": {
        "deepseek-v4-flash": {
          "name": "DeepSeek V4 Flash",
          "cost": { "input": 0.3, "output": 1.2, "cache_read": 0.006 },
          "limit": { "context": 1000000 },
          "modalities": { "input": ["text"] }
        }
      }},
      "opencode": { "models": {
        "gpt-5.1-codex-max": {
          "name": "GPT-5.1 Codex Max",
          "cost": { "input": 2, "output": 10 },
          "limit": { "context": 400000, "output": 128000 },
          "modalities": { "input": ["text", "image"] }
        }
      }},
      "github-copilot": { "models": {
        "claude-sonnet-4.6": {
          "name": "Claude Sonnet 4.6",
          "cost": { "input": 3, "output": 15 },
          "limit": { "context": 1000000, "output": 128000 },
          "modalities": { "input": ["text", "image"] }
        }
      }},
      "azure": { "models": { "gpt-5": { "name": "GPT-5" } } }
    }
    "#;

    fn catalog() -> Catalog {
        Catalog::parse(FIXTURE).expect("the fixture parses")
    }

    #[test]
    fn parse_keeps_the_wired_providers_and_drops_the_rest() {
        let catalog = catalog();
        assert!(catalog.has("anthropic"));
        assert!(catalog.has("github-copilot"));
        // Azure is in api.json but no config spelling reads it.
        assert!(!catalog.has("azure"));
        // The kept rows still answer.
        assert!(catalog.facts("anthropic", "claude-sonnet-4-6").is_some());
    }

    #[test]
    fn facts_carry_price_limits_and_vision() {
        let facts = catalog()
            .facts("anthropic", "claude-sonnet-4-6")
            .expect("the row is there");
        assert_eq!(facts.display_name.as_deref(), Some("Claude Sonnet 4.6"));
        assert_eq!(facts.pricing.expect("priced").input_per_mtok, 3.0);
        assert_eq!(
            facts.pricing.expect("priced").cached_input_per_mtok,
            Some(0.3)
        );
        assert_eq!(facts.input_token_limit, Some(1_000_000));
        assert_eq!(facts.output_token_limit, Some(128_000));
        assert_eq!(facts.supports_vision, Some(true));
        assert_eq!(facts.supports_tools, Some(true));
        assert_eq!(facts.knowledge_cutoff.as_deref(), Some("2025-08-31"));
    }

    #[test]
    fn a_prompt_ceiling_narrower_than_the_context_window_wins() {
        let facts = catalog().facts("openai", "gpt-5.1-codex").unwrap();
        // OpenAI publishes 272K input inside a 400K context window.
        assert_eq!(facts.input_token_limit, Some(272_000));
        assert_eq!(facts.output_token_limit, Some(128_000));
    }

    #[test]
    fn a_text_only_row_answers_no_vision_and_an_absent_modality_block_stays_unknown() {
        let catalog = catalog();
        assert_eq!(
            catalog
                .facts("zai", "glm-4.5")
                .and_then(|facts| facts.supports_vision),
            Some(false)
        );
        // The fixture's mistral row has no modalities: unknown, not no.
        assert_eq!(
            catalog
                .facts("mistral", "mistral-large-4")
                .and_then(|facts| facts.supports_vision),
            None
        );
    }

    #[test]
    fn the_lookup_falls_through_case_alias_and_slug() {
        let catalog = catalog();
        // Case: Z.ai's endpoint spells the ids lowercase, its docs
        // uppercase.
        assert!(catalog.facts("zai", "GLM-4.5").is_some());
        // Alias: Rhema's DeepSeek default is the moving id.
        assert_eq!(
            catalog
                .facts("deepseek", "deepseek-chat")
                .and_then(|facts| facts.display_name)
                .as_deref(),
            Some("DeepSeek V4 Flash")
        );
        // Alias: the stored Mistral spelling drops the trailing version.
        assert!(catalog.facts("mistral", "mistral-large-4-0").is_some());
        // Slug: a preview suffix names the stable row's facts.
        assert!(
            catalog
                .facts("anthropic", "claude-sonnet-4.6-preview")
                .is_some()
        );
    }

    #[test]
    fn a_missing_row_falls_back_for_facts_but_never_for_a_price() {
        let catalog = catalog();
        // The fixture's openai block has no Codex Max row; opencode
        // does, and the gateway's resale price must not become
        // OpenAI's bill.
        let facts = catalog
            .facts("openai", "gpt-5.1-codex-max")
            .expect("the fallback row answers");
        assert_eq!(facts.input_token_limit, Some(400_000));
        assert_eq!(facts.supports_vision, Some(true));
        assert_eq!(facts.pricing, None);
        // The subscription route reads the same fallback and still
        // drops the price.
        assert_eq!(
            catalog
                .facts("chatgpt", "gpt-5.1-codex-max")
                .and_then(|facts| facts.pricing),
            None
        );
    }

    #[test]
    fn a_subscription_route_keeps_the_facts_and_drops_the_price() {
        let catalog = catalog();
        let metered = catalog.facts("openai", "gpt-5.1-codex").unwrap();
        let subscription = catalog.facts("chatgpt", "gpt-5.1-codex").unwrap();
        assert!(metered.pricing.is_some());
        assert_eq!(subscription.pricing, None);
        assert_eq!(subscription.input_token_limit, Some(272_000));
        assert_eq!(subscription.supports_vision, Some(true));
    }

    #[test]
    fn an_uncovered_provider_or_id_reads_as_unknown() {
        let catalog = catalog();
        assert!(catalog.facts("llama-cpp", "local").is_none());
        assert!(catalog.facts("anthropic", "claude-not-listed").is_none());
    }

    #[test]
    fn decorate_fills_blanks_and_never_overwrites_a_listing_value() {
        let mut models = vec![
            // The adapter priced this one itself: its number stands.
            card("claude-sonnet-4-6", Some(9.0), None, None),
            // Nothing listed this id: the card stays as it came.
            card("claude-opus-4-1", None, None, None),
        ];
        catalog().decorate("anthropic", &mut models);
        assert_eq!(models[0].pricing.unwrap().input_per_mtok, 9.0);
        assert_eq!(models[1].pricing, None);
        assert_eq!(models[1].display_name, "claude-opus-4-1");
    }

    #[test]
    fn decorate_replaces_an_id_spelled_display_name_and_fills_limits() {
        let mut models = vec![card("gpt-5.1-codex", None, None, None)];
        catalog().decorate("openai", &mut models);
        assert_eq!(models[0].display_name, "GPT-5.1 Codex");
        assert_eq!(models[0].input_token_limit, Some(272_000));
        assert_eq!(models[0].supports_vision, Some(true));
        assert_eq!(models[0].pricing.unwrap().output_per_mtok, 8.5);
    }

    fn card(
        id: &str,
        input_per_mtok: Option<f64>,
        limit: Option<u64>,
        vision: Option<bool>,
    ) -> rhema_provider::ModelCard {
        rhema_provider::ModelCard {
            id: id.to_owned(),
            display_name: id.to_owned(),
            description: String::new(),
            input_token_limit: limit,
            output_token_limit: None,
            supports_vision: vision,
            generation_methods: vec!["generateContent".to_owned()],
            pricing: input_per_mtok.map(|input| Pricing {
                input_per_mtok: input,
                output_per_mtok: 1.0,
                cached_input_per_mtok: None,
            }),
        }
    }
}
