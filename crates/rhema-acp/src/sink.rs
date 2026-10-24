//! The event bridge: [`rhema_core::Event`] in, `session/update` out.
//!
//! The sink owns the turn's streaming surface. Text and thinking deltas
//! stream as message chunks; a tool call is announced pending when it
//! starts, its arguments accumulate silently (ACP has no arg deltas —
//! `rawInput` is set once, complete), and the result closes the call
//! with a title built from the assembled arguments, so editors show the
//! actual command instead of the tool name. A running command's output
//! streams as in-progress updates, one per completed line; the full
//! text lands again with the result, so clients that replace content
//! per update still end on the whole output.
//!
//! The sink also meters what the session spends: `Event::StepEnd`
//! carries the step's token usage, which lands in the session's shared
//! [`SessionMeter`] and on the wire as a `UsageUpdate` — tokens in
//! context, the model's window, cumulative cost — whenever the model
//! card knows the window. `serve` reads the same meter to attach the
//! session's cumulative totals to the `session/prompt` response.

use std::sync::{Arc, Mutex};

use agent_client_protocol::schema::v1::{
    ContentBlock, ContentChunk, Cost, SessionNotification, SessionUpdate, TextContent, ToolCall,
    ToolCallContent, ToolCallId, ToolCallLocation, ToolCallStatus, ToolCallUpdate,
    ToolCallUpdateFields, ToolKind, UsageUpdate,
};
use agent_client_protocol::{Client, ConnectionTo};
use rhema_core::{Event, EventSink, Usage};
use rhema_provider::Pricing;

/// Cumulative token and cost accounting for one session, shared by the
/// turns that run on it: each step adds its usage, each turn reads the
/// totals for its `session/prompt` response. The numbers are the
/// session's, not the turn's — a turn's steps re-read the growing
/// history, so per-turn inputs would double-count the conversation.
#[derive(Debug, Default)]
pub(crate) struct SessionMeter {
    /// Prompt tokens billed across all steps.
    input_tokens: u64,
    /// Completion tokens billed across all steps.
    output_tokens: u64,
    /// The cached subset of those prompts.
    cached_read_tokens: u64,
    /// USD the session has cost, at the card's rates.
    cost_usd: f64,
    /// Tokens the most recent step left in context — its prompt plus
    /// its reply, the shape of the next request. Zero before the first
    /// step.
    context_used: u64,
}

impl SessionMeter {
    /// Records one step's usage and prices it.
    pub(crate) fn record(&mut self, usage: &Usage, pricing: Option<&Pricing>) {
        let input = u64::from(usage.input_tokens);
        let output = u64::from(usage.output_tokens);
        let cached = u64::from(usage.cached_input_tokens);
        self.input_tokens += input;
        self.output_tokens += output;
        self.cached_read_tokens += cached;
        self.context_used = input + output;
        if let Some(pricing) = pricing {
            self.cost_usd += pricing.cost(input, output, cached);
        }
    }

    /// The session's cumulative totals, on the ACP usage shape: totals
    /// across steps, cached prompts under the read key (rhema tracks no
    /// cache writes and no thought tokens separately).
    pub(crate) fn totals(&self) -> (u64, u64, u64, u64, f64) {
        (
            self.input_tokens + self.output_tokens,
            self.input_tokens,
            self.output_tokens,
            self.cached_read_tokens,
            self.cost_usd,
        )
    }

    /// Tokens in context after the most recent step.
    pub(crate) fn context_used(&self) -> u64 {
        self.context_used
    }
}

/// Forwards engine events to the client as session updates.
pub(crate) struct AcpSink {
    cx: ConnectionTo<Client>,
    session_id: agent_client_protocol::schema::v1::SessionId,
    /// The in-flight call: id, name, and its assembled JSON arguments.
    pending: Option<(String, String, String)>,
    /// The session's shared meter; every step lands here.
    meter: Arc<Mutex<SessionMeter>>,
    /// The current model's context window, when its card knows one; a
    /// `UsageUpdate` needs both sides of the fraction, so without a
    /// window no usage update is sent.
    context_size: Option<u64>,
    /// The current model's prices, when its card carries them.
    pricing: Option<Pricing>,
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
            meter: Arc::new(Mutex::new(SessionMeter::default())),
            context_size: None,
            pricing: None,
        }
    }

    /// Wires the sink to the session's shared meter and the current
    /// model's window and prices, so steps surface as usage updates.
    pub(crate) fn measuring(
        mut self,
        meter: Arc<Mutex<SessionMeter>>,
        context_size: Option<u64>,
        pricing: Option<Pricing>,
    ) -> Self {
        self.meter = meter;
        self.context_size = context_size;
        self.pricing = pricing;
        self
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
                        .kind(tool_kind(name)),
                ));
            }
            Event::ToolCallArgsDelta { id, delta } => {
                // The args accumulate for the announced call; a fragment
                // naming a different call has no slot in the single
                // pending update, the same discipline the result path's
                // id check applies.
                if let Some((pending_id, _, args)) = &mut self.pending {
                    if *pending_id == id {
                        args.push_str(delta);
                    }
                }
            }
            // A running command's completed lines stream as in-progress
            // updates; the full text arrives again with the result, so
            // a client that replaces content per update still ends on
            // the whole output.
            Event::ToolOutputDelta { id, chunk } => {
                self.send(SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                    ToolCallId::new(id),
                    ToolCallUpdateFields::new()
                        .status(ToolCallStatus::InProgress)
                        .content(vec![ToolCallContent::from(text_block(chunk))]),
                )));
            }
            // The protocol has no free-form status slot on a tool call;
            // the phase a streaming tool reports has nowhere to land.
            Event::ToolStatus { .. } => {}
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
                let fields = ToolCallUpdateFields::new()
                    .title(title)
                    .kind(tool_kind(name.as_deref().unwrap_or_default()))
                    .status(if is_error {
                        ToolCallStatus::Failed
                    } else {
                        ToolCallStatus::Completed
                    })
                    .content(vec![ToolCallContent::from(text_block(content))])
                    .raw_input(
                        args.as_deref()
                            .map(|args| serde_json::from_str(args).unwrap_or_else(|_| args.into())),
                    )
                    .raw_output(code.map(|exit| serde_json::json!({ "exitCode": exit })));
                let fields = match args.as_deref().and_then(locations_of) {
                    Some(locations) => fields.locations(locations),
                    None => fields,
                };
                self.send(SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                    ToolCallId::new(id),
                    fields,
                )));
            }
            // Step boundaries meter the session: the usage lands in the
            // shared meter and, when the model's window is known, on
            // the wire as a context and cost update.
            Event::StepEnd { usage, .. } => {
                let (used, cost) = {
                    let mut meter = self
                        .meter
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    meter.record(&usage, self.pricing.as_ref());
                    (meter.context_used(), meter.cost_usd)
                };
                if let Some(size) = self.context_size {
                    let mut update = UsageUpdate::new(used, size);
                    if cost > 0.0 {
                        update = update.cost(Cost::new(cost, "USD"));
                    }
                    self.send(SessionUpdate::UsageUpdate(update));
                }
            }
            // The step's start is the prompt request's concern. The
            // running usage tally stays unreported: the ACP UsageUpdate
            // rides the step's close, where the tally is final.
            Event::Usage(_) => {}
            Event::StepStart { .. } => {}
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

/// The files a tool call touches, read from its assembled arguments:
/// every path-shaped key the toolkit's tools use maps to a location the
/// editor can offer navigation to. No path-shaped key, no locations.
fn locations_of(args: &str) -> Option<Vec<ToolCallLocation>> {
    let parsed: serde_json::Value = serde_json::from_str(args).ok()?;
    let paths: Vec<ToolCallLocation> = ["path", "file_path", "file"]
        .iter()
        .filter_map(|key| parsed.get(*key)?.as_str())
        .map(|path| ToolCallLocation::new(std::path::PathBuf::from(path)))
        .collect();
    (!paths.is_empty()).then_some(paths)
}

/// The kind a tool call renders as. The coding agent ships `bash` (a
/// command) and `zoom` (a read over the session's transcript view);
/// anything a deployment adds falls back to other.
pub(crate) fn tool_kind(name: &str) -> ToolKind {
    match name {
        "bash" => ToolKind::Execute,
        "zoom" => ToolKind::Read,
        _ => ToolKind::Other,
    }
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

    #[test]
    fn paths_in_the_args_become_locations() {
        assert_eq!(
            locations_of(r#"{"path":"/tmp/a.rs"}"#).map(|locations| locations.len()),
            Some(1)
        );
        assert_eq!(locations_of(r#"{"command":"ls"}"#).map(|l| l.len()), None);
        assert_eq!(locations_of("not json"), None);
    }

    #[test]
    fn the_meter_sums_steps_and_prices_them() {
        let pricing = Pricing {
            input_per_mtok: 1.0,
            output_per_mtok: 3.0,
            cached_input_per_mtok: None,
        };
        let mut meter = SessionMeter::default();
        meter.record(&Usage::default(), Some(&pricing));
        assert_eq!(meter.context_used(), 0);
        meter.record(
            &Usage {
                input_tokens: 1_000,
                output_tokens: 100,
                cached_input_tokens: 400,
            },
            Some(&pricing),
        );
        // 600 uncached input at $1/M + 400 cached at the $0.25 fallback
        // + 100 output at $3/M: 0.0006 + 0.0001 + 0.0003.
        let (total, input, output, cached_read, cost) = meter.totals();
        assert_eq!(
            (total, input, output, cached_read),
            (1_100, 1_000, 100, 400)
        );
        assert!((cost - 0.001).abs() < 1e-9, "{cost}");
        assert_eq!(meter.context_used(), 1_100);
    }
}
