//! Conversation content: roles, content blocks, and messages.
//!
//! A [`Message`] is a role plus an ordered list of [`Content`] blocks, so one
//! assistant turn can mix text and several [`ToolCall`]s. All types are
//! `serde`-friendly: the driver must be able to checkpoint a session
//! containing them (SPEC.md §6).

use serde::{Deserialize, Serialize};

use crate::tool::{ToolCall, ToolResult};

/// Who authored a [`Message`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// System prompt.
    System,
    /// Human turn, or a driver-injected inbox item.
    User,
    /// Model turn.
    Assistant,
    /// Tool result, answering a prior [`ToolCall`].
    Tool,
}

/// One block of a message's content.
///
/// Serialized untagged: `Text` is a bare JSON string, the tool blocks are
/// objects whose field sets distinguish them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Content {
    /// Plain text.
    Text(String),
    /// A tool invocation requested by the model.
    ToolCall(ToolCall),
    /// A result answering a prior [`ToolCall`].
    ToolResult(ToolResult),
}

/// One turn of the conversation, authored by [`Role`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    /// Author of this turn.
    pub role: Role,
    /// Ordered content blocks; a single turn may mix text and tool blocks.
    pub content: Vec<Content>,
}

impl Message {
    /// A single-text-block message.
    pub fn text(role: Role, text: impl Into<String>) -> Self {
        Self {
            role,
            content: vec![Content::Text(text.into())],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::ToolCall;

    #[test]
    fn message_round_trips_through_serde() {
        let message = Message {
            role: Role::Assistant,
            content: vec![
                Content::Text("running the check".into()),
                Content::ToolCall(ToolCall {
                    id: "call_1".into(),
                    name: "cargo_test".into(),
                    args: serde_json::json!({ "package": "rhema-core" }),
                }),
            ],
        };
        let json = serde_json::to_string(&message).unwrap();
        let back: Message = serde_json::from_str(&json).unwrap();
        assert_eq!(back, message);
    }
}
