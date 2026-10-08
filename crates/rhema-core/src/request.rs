//! What goes to the model: the assembled request and its parameters.

use serde::{Deserialize, Serialize};

use crate::message::Message;

/// A tool advertised to the model this step, straight from
/// [`crate::ToolResolver::specs`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolSpec {
    /// Name the model uses to call the tool.
    pub name: String,
    /// What the tool does; the model picks tools from this.
    pub description: String,
    /// JSON schema for the call arguments.
    pub parameters: serde_json::Value,
}

impl ToolSpec {
    /// A spec with a JSON-schema object of no required properties.
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        parameters: serde_json::Value,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            parameters,
        }
    }
}

/// How much reasoning the model should do, mapped by each provider onto
/// its own request parameter — OpenAI-compatible `reasoning_effort`,
/// OpenRouter's `reasoning.effort`, Anthropic's thinking budget, Gemini's
/// thinking budget. [`ThinkingEffort::Default`] sends nothing: the
/// server's own default stands, which keeps strict endpoints happy and
/// leaves always-thinking models as they are.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ThinkingEffort {
    /// The provider decides; rhema sends no effort parameter.
    #[default]
    Default,
    /// Light reasoning — quick turns, cheap models.
    Low,
    /// The middle setting.
    Medium,
    /// Maximum reasoning effort.
    High,
}

impl ThinkingEffort {
    /// The level's name as the config, the flag, and the command spell it.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        }
    }

    /// Parses a level name; `None` for anything else.
    pub fn parse(text: &str) -> Option<Self> {
        match text.trim().to_lowercase().as_str() {
            "default" => Some(Self::Default),
            "low" => Some(Self::Low),
            "medium" => Some(Self::Medium),
            "high" => Some(Self::High),
            _ => None,
        }
    }
}

/// Sampling parameters for the model call.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct ChatParams {
    /// Model identifier, provider-interpreted (e.g. `gemini-2.5-flash`).
    pub model: String,
    /// Sampling temperature, if the driver wants one.
    pub temperature: Option<f32>,
    /// Output token ceiling, if the driver wants one.
    pub max_tokens: Option<u32>,
    /// How much reasoning the model should do. Defaults to
    /// [`ThinkingEffort::Default`] so pre-seam payloads load unchanged.
    #[serde(default)]
    pub thinking: ThinkingEffort,
}

impl ChatParams {
    /// Parameters naming `model`, with defaults for everything else.
    pub fn new(model: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            ..Default::default()
        }
    }
}

/// The full request handed to a [`crate::ModelProvider`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelRequest {
    /// System prompt, if the agent has one.
    pub system: Option<String>,
    /// The conversation so far, cloned from the session at step start.
    pub messages: Vec<Message>,
    /// Tools offered this step.
    pub tools: Vec<ToolSpec>,
    /// Sampling parameters.
    pub params: ChatParams,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn levels_parse_and_round_trip_through_serde() {
        assert_eq!(ThinkingEffort::parse(" High "), Some(ThinkingEffort::High));
        assert_eq!(ThinkingEffort::parse("low"), Some(ThinkingEffort::Low));
        assert_eq!(ThinkingEffort::parse("maximal"), None);
        assert_eq!(ThinkingEffort::default(), ThinkingEffort::Default);

        let params = ChatParams {
            thinking: ThinkingEffort::High,
            ..ChatParams::new("m")
        };
        let json = serde_json::to_string(&params).unwrap();
        assert!(json.contains("\"thinking\":\"high\""), "{json}");
        assert_eq!(serde_json::from_str::<ChatParams>(&json).unwrap(), params);
        // A payload from before the field existed loads as Default.
        let legacy: ChatParams = serde_json::from_str(r#"{"model":"m"}"#).unwrap();
        assert_eq!(legacy.thinking, ThinkingEffort::Default);
    }
}
