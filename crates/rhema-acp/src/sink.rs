//! The event bridge: [`rhema_core::Event`] in, `session/update` out.
//!
//! The sink owns the turn's streaming surface. Text and thinking deltas
//! stream as message chunks; a tool call is announced pending when it
//! starts, its arguments accumulate silently (ACP has no arg deltas —
//! `rawInput` is set once, complete), and the result closes the call
//! with a title built from the assembled arguments, so editors show the
//! actual command instead of the tool name.

use agent_client_protocol::schema::v1::{
    ContentBlock, ContentChunk, SessionNotification, SessionUpdate, TextContent, ToolCall,
    ToolCallContent, ToolCallId, ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields, ToolKind,
};
use agent_client_protocol::{Client, ConnectionTo};
use rhema_core::{Event, EventSink};

/// Forwards engine events to the client as session updates.
pub(crate) struct AcpSink {
    cx: ConnectionTo<Client>,
    session_id: agent_client_protocol::schema::v1::SessionId,
    /// The in-flight call: id, name, and its assembled JSON arguments.
    pending: Option<(String, String, String)>,
}

impl AcpSink {
    /// A sink bound to one session on one connection.
    pub(crate) fn new(
        cx: ConnectionTo<Client>,
        session_id: agent_client_protocol::schema::v1::SessionId,
    ) -> Self {
        Self {
            cx,
            session_id,
            pending: None,
        }
    }

    fn send(&mut self, update: SessionUpdate) {
        // A send fails only when the connection is gone; the turn then
        // unwinds with the connection task. Dropping the update is all
        // there is to do.
        let _ = self
            .cx
            .send_notification(SessionNotification::new(self.session_id.clone(), update));
    }
}

impl EventSink for AcpSink {
    fn emit(&mut self, event: Event<'_>) {
        match event {
            Event::TextDelta(delta) => self.send(SessionUpdate::AgentMessageChunk(
                ContentChunk::new(text_block(delta)),
            )),
            Event::ThinkingDelta(delta) => self.send(SessionUpdate::AgentThoughtChunk(
                ContentChunk::new(text_block(delta)),
            )),
            Event::ToolCallStart { id, name } => {
                self.pending = Some((id.to_owned(), name.to_owned(), String::new()));
                self.send(SessionUpdate::ToolCall(
                    ToolCall::new(ToolCallId::new(id), name)
                        .name(name)
                        .kind(ToolKind::Execute),
                ));
            }
            Event::ToolCallArgsDelta(delta) => {
                if let Some((_, _, args)) = &mut self.pending {
                    args.push_str(delta);
                }
            }
            // A running tool's live output has no ACP update of its own;
            // the full text arrives with the result.
            Event::ToolOutputDelta { .. } => {}
            Event::ToolResult {
                id,
                content,
                is_error,
                code,
            } => {
                // The title is the command when the arguments carry one.
                let pending = self.pending.take_if(|(pending_id, _, _)| pending_id == id);
                let (name, args) = pending
                    .map(|(_, name, args)| (Some(name), Some(args)))
                    .unwrap_or((None, None));
                let title = args
                    .as_deref()
                    .and_then(command_of)
                    .unwrap_or_else(|| name.clone().unwrap_or_else(|| "tool".to_owned()));
                self.send(SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                    ToolCallId::new(id),
                    ToolCallUpdateFields::new()
                        .title(title)
                        .kind(ToolKind::Execute)
                        .status(if is_error {
                            ToolCallStatus::Failed
                        } else {
                            ToolCallStatus::Completed
                        })
                        .content(vec![ToolCallContent::from(text_block(content))])
                        .raw_input(args.map(|args| {
                            serde_json::from_str(&args).unwrap_or_else(|_| args.into())
                        }))
                        .raw_output(code.map(|exit| serde_json::json!({ "exitCode": exit }))),
                )));
            }
            // Step boundaries are the prompt request's concern.
            Event::StepStart { .. } | Event::StepEnd { .. } => {}
        }
    }
}

fn text_block(text: &str) -> ContentBlock {
    ContentBlock::Text(TextContent::new(text))
}

/// The command a bash call runs: the single `command` string argument.
fn command_of(args: &str) -> Option<String> {
    let parsed: serde_json::Value = serde_json::from_str(args).ok()?;
    parsed.get("command")?.as_str().map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_title_is_the_command_when_the_args_carry_one() {
        assert_eq!(
            command_of(r#"{"command":"cargo test --workspace"}"#).as_deref(),
            Some("cargo test --workspace")
        );
        assert_eq!(command_of("not json"), None);
        assert_eq!(command_of(r#"{"other":1}"#), None);
    }
}
