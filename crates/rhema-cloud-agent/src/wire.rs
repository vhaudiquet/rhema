//! The JSON protocol between a cloud frontend and its client.
//!
//! One small, vendor-free surface both frontends speak, so a client
//! written against one talks to the other: what the client sends
//! ([`ClientMessage`] — a turn's input, an approval answer, a stop),
//! and what streams back ([`ServerMessage`] — the engine's events as
//! they arrive, the approval round-trip, the turn's close). The shapes
//! are the Cloudflare DO's existing wire, typed; nothing changed on
//! the socket.
#![forbid(unsafe_code)]

use rhema_core::{Event, StopReason, ToolCall, Usage};

/// What a connected client sends.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMessage {
    /// Start one turn with this user text.
    Chat {
        /// The user's text; one message per turn.
        text: String,
    },
    /// Answer a pending approval.
    Approve {
        /// The call id the request carried.
        id: String,
        /// The answer: run the call, or deny it.
        approve: bool,
    },
    /// Ask the running turn to stop.
    Stop,
}

/// What the frontend streams back: the engine's live events, the
/// approval round-trip, and the turn's close.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMessage {
    /// The step begins; `iteration` counts the steps this agent has
    /// already completed.
    StepStart {
        /// Zero-based index of this step within the agent.
        iteration: u32,
    },
    /// A piece of the model's reasoning trace.
    ThinkingDelta {
        /// The piece, in stream order.
        delta: String,
    },
    /// A piece of assistant text.
    TextDelta {
        /// The piece, in stream order.
        delta: String,
    },
    /// A tool call begins streaming.
    ToolCallStart {
        /// The call's correlation id.
        id: String,
        /// The tool's name.
        name: String,
    },
    /// A piece of a tool call's JSON arguments.
    ToolCallArgsDelta {
        /// The streaming call's correlation id.
        id: String,
        /// The piece, in stream order.
        delta: String,
    },
    /// A chunk of a running tool's human-facing output.
    ToolOutputDelta {
        /// The running call's correlation id.
        id: String,
        /// The chunk, in stream order.
        chunk: String,
    },
    /// A tool result was appended to the session history.
    ToolResult {
        /// The answered call's id.
        id: String,
        /// The result payload.
        content: String,
        /// Whether the call failed.
        is_error: bool,
        /// The process exit code of command-like tools; `None`
        /// otherwise.
        code: Option<i32>,
    },
    /// The step ends.
    StepEnd {
        /// Token accounting for the model call.
        usage: Usage,
        /// Why the model stopped.
        stop_reason: StopReason,
    },
    /// A gated call waits for the client's decision — answer with
    /// [`ClientMessage::Approve`], or the ask denies at the timeout.
    ApprovalRequest {
        /// The call's correlation id; the answer quotes it back.
        id: String,
        /// The tool that wants to run.
        tool: String,
        /// The command text, for the prompt the client shows.
        command: String,
    },
    /// A [`Self::ApprovalRequest`] timed out and was denied.
    ApprovalExpired {
        /// The call id whose ask expired.
        id: String,
    },
    /// A request was refused or failed before a turn — a second chat
    /// while one runs, an unreadable message.
    Error {
        /// What went wrong, client-renderable.
        message: String,
    },
    /// The turn is over, in either direction: the close every client
    /// waits for. Exactly one of the two shapes carries fields — the
    /// success shape `stop_reason`/`usage`/`cost_usd`, the failure
    /// shape `error`.
    TurnComplete {
        /// Why the model stopped; absent when the turn failed.
        #[serde(skip_serializing_if = "Option::is_none")]
        stop_reason: Option<StopReason>,
        /// The turn's token accounting; absent when the turn failed.
        #[serde(skip_serializing_if = "Option::is_none")]
        usage: Option<Usage>,
        /// The session's cumulative cost in dollars; absent when the
        /// turn failed.
        #[serde(skip_serializing_if = "Option::is_none")]
        cost_usd: Option<f64>,
        /// What went wrong; absent on success.
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
}

impl ServerMessage {
    /// The wire form of one engine event — the sink's mapping, typed.
    pub fn from_event(event: Event<'_>) -> Self {
        match event {
            Event::StepStart { iteration } => Self::StepStart { iteration },
            Event::ThinkingDelta(delta) => Self::ThinkingDelta {
                delta: delta.to_owned(),
            },
            Event::TextDelta(delta) => Self::TextDelta {
                delta: delta.to_owned(),
            },
            Event::ToolCallStart { id, name } => Self::ToolCallStart {
                id: id.to_owned(),
                name: name.to_owned(),
            },
            Event::ToolCallArgsDelta { id, delta } => Self::ToolCallArgsDelta {
                id: id.to_owned(),
                delta: delta.to_owned(),
            },
            Event::ToolOutputDelta { id, chunk } => Self::ToolOutputDelta {
                id: id.to_owned(),
                chunk: chunk.to_owned(),
            },
            Event::ToolResult {
                id,
                content,
                is_error,
                code,
            } => Self::ToolResult {
                id: id.to_owned(),
                content: content.to_owned(),
                is_error,
                code,
            },
            Event::StepEnd { usage, stop_reason } => Self::StepEnd {
                usage,
                stop_reason: stop_reason.clone(),
            },
        }
    }

    /// The success close for a turn that ended with `output`.
    pub fn turn_complete(stop_reason: StopReason, usage: Usage, cost_usd: f64) -> Self {
        Self::TurnComplete {
            stop_reason: Some(stop_reason),
            usage: Some(usage),
            cost_usd: Some(cost_usd),
            error: None,
        }
    }

    /// The failure close for a turn that errored — the history is
    /// still whole, so the client may continue the conversation.
    pub fn turn_failed(error: String) -> Self {
        Self::TurnComplete {
            stop_reason: None,
            usage: None,
            cost_usd: None,
            error: Some(error),
        }
    }
}

/// The command text of a call, for the prompt the client shows.
pub fn approval_command(call: &ToolCall) -> String {
    call.args
        .get("command")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bash_call() -> ToolCall {
        ToolCall {
            id: "call-1".into(),
            name: "bash".into(),
            args: serde_json::json!({ "command": "cargo test" }),
            provider_data: serde_json::Value::Null,
        }
    }

    #[test]
    fn client_messages_round_trip() {
        for text in [
            r#"{"type":"chat","text":"hi"}"#,
            r#"{"type":"approve","id":"call-1","approve":true}"#,
            r#"{"type":"stop"}"#,
        ] {
            let message: ClientMessage = serde_json::from_str(text).unwrap();
            assert_eq!(
                serde_json::to_value(&message).unwrap(),
                serde_json::from_str::<serde_json::Value>(text).unwrap(),
            );
        }
    }

    #[test]
    fn events_map_field_for_field() {
        let event = Event::ToolResult {
            id: "call-1",
            content: "ok",
            is_error: false,
            code: Some(0),
        };
        assert_eq!(
            serde_json::to_value(ServerMessage::from_event(event)).unwrap(),
            serde_json::json!({
                "type": "tool_result", "id": "call-1", "content": "ok",
                "is_error": false, "code": 0,
            }),
        );
    }

    #[test]
    fn the_approval_shapes_match_the_wire() {
        let call = bash_call();
        assert_eq!(
            serde_json::to_value(ServerMessage::ApprovalRequest {
                id: call.id.clone(),
                tool: call.name.clone(),
                command: approval_command(&call),
            })
            .unwrap(),
            serde_json::json!({
                "type": "approval_request", "id": "call-1",
                "tool": "bash", "command": "cargo test",
            }),
        );
        assert_eq!(
            serde_json::to_value(ServerMessage::ApprovalExpired {
                id: "call-1".into(),
            })
            .unwrap(),
            serde_json::json!({ "type": "approval_expired", "id": "call-1" }),
        );
    }

    #[test]
    fn the_close_has_exactly_one_shape() {
        let ok = ServerMessage::turn_complete(StopReason::EndTurn, Usage::default(), 0.5);
        let ok = serde_json::to_value(&ok).unwrap();
        assert_eq!(
            ok,
            serde_json::json!({
                "type": "turn_complete", "stop_reason": "end_turn",
                "usage": { "input_tokens": 0, "output_tokens": 0, "cached_input_tokens": 0 },
                "cost_usd": 0.5,
            }),
        );
        assert_eq!(
            ok.get("error"),
            None,
            "the success shape carries no error key"
        );
        let failed = ServerMessage::turn_failed("boom".into());
        let failed = serde_json::to_value(&failed).unwrap();
        assert_eq!(
            failed,
            serde_json::json!({ "type": "turn_complete", "error": "boom" }),
        );
    }

    #[test]
    fn a_bash_call_without_a_command_carries_an_empty_command() {
        let mut call = bash_call();
        call.args = serde_json::json!({});
        assert_eq!(approval_command(&call), "");
    }

    #[test]
    fn a_message_with_content_map_deserializes_strictly() {
        // The tag decides: an unknown type is an error, not a silent
        // variant.
        assert!(serde_json::from_str::<ClientMessage>("{\"type\":\"nope\"}").is_err());
    }
}
