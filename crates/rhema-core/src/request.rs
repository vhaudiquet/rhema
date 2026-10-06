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

/// Sampling parameters for the model call.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct ChatParams {
    /// Model identifier, provider-interpreted (e.g. `gemini-2.5-flash`).
    pub model: String,
    /// Sampling temperature, if the driver wants one.
    pub temperature: Option<f32>,
    /// Output token ceiling, if the driver wants one.
    pub max_tokens: Option<u32>,
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
