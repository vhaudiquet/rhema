//! The vocabulary translation between the engine and the wire: effort
//! and stop-reason mappings, the ACP command/select labels, the usage
//! and timestamp shapes, and the event replay a load answers with.

use super::*;

/// The reasoning effort a switch value names. `Default` parses
/// nowhere: over ACP it is not exposed, so a client cannot set it.
pub(crate) fn effort_of(value: &str) -> Option<ThinkingEffort> {
    match value.trim().to_ascii_lowercase().as_str() {
        "low" => Some(ThinkingEffort::Low),
        "medium" => Some(ThinkingEffort::Medium),
        "high" => Some(ThinkingEffort::High),
        _ => None,
    }
}

/// A reasoning effort's value id, on the wire and in commands.
pub(crate) fn effort_id(effort: ThinkingEffort) -> &'static str {
    match effort {
        ThinkingEffort::Default => "default",
        ThinkingEffort::Low => "low",
        ThinkingEffort::Medium => "medium",
        ThinkingEffort::High => "high",
    }
}

/// A reasoning effort's display name, capitalized where the wire id
/// stays lowercase.
pub(crate) fn effort_name(effort: ThinkingEffort) -> &'static str {
    match effort {
        ThinkingEffort::Default => "Default",
        ThinkingEffort::Low => "Low",
        ThinkingEffort::Medium => "Medium",
        ThinkingEffort::High => "High",
    }
}

/// The effort a client sees. `Default` is not among the exposed
/// levels, so it reads as the level an unset one resolves to.
pub(crate) fn exposed(effort: ThinkingEffort) -> ThinkingEffort {
    match effort {
        ThinkingEffort::Default => ThinkingEffort::Medium,
        named => named,
    }
}

/// A prompt's title: its first non-empty line, kept to a width a
/// session list can render.
pub(crate) fn title_of(prompt: &str) -> Option<String> {
    const MAX: usize = 77;
    let line = prompt
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())?;
    let count = line.chars().count();
    let mut title: String = line.chars().take(MAX).collect();
    if count > MAX {
        title.push_str("...");
    }
    Some(title)
}

/// The session's cumulative totals on the ACP usage shape: totals
/// across steps, cached prompts under the read key. `None` when the
/// session vanished under the response.
pub(crate) fn session_usage(
    app: &App,
    id: &str,
) -> Option<agent_client_protocol::schema::v1::Usage> {
    let meter = app.meter(id)?;
    let meter = meter
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let (total, input, output, cached_read, _cost) = meter.totals();
    Some(
        agent_client_protocol::schema::v1::Usage::new(total, input, output)
            .cached_read_tokens(cached_read),
    )
}

/// The prompt response a turn ends with, carrying the session's
/// cumulative usage when the meter still stands.
pub(crate) fn prompt_response(stop: StopReason, app: &App, id: &str) -> PromptResponse {
    let response = PromptResponse::new(stop);
    match session_usage(app, id) {
        Some(usage) => response.usage(usage),
        None => response,
    }
}

/// The wall clock, RFC 3339 with milliseconds, UTC: the timestamp
/// shape `SessionInfoUpdate` documents. No time dependency — the
/// civil-from-days arithmetic is the whole trick.
pub(crate) fn rfc3339_now() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    rfc3339(now.as_secs(), now.subsec_millis())
}

pub(crate) fn rfc3339(secs: u64, millis: u32) -> String {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{millis:03}Z",
        rem / 3_600,
        (rem % 3_600) / 60,
        rem % 60
    )
}

/// The calendar date of `days` since 1970-01-01 (Howard Hinnant's
/// civil_from_days; valid for the whole u64 second range).
pub(crate) fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if month <= 2 { year + 1 } else { year }, month, day)
}

/// A model select choice's label: the display name when the listing
/// knows one, the id otherwise.
pub(crate) fn option_name(card: &rhema_provider::ModelCard) -> String {
    if card.display_name.is_empty() {
        card.id.clone()
    } else {
        card.display_name.clone()
    }
}

/// A model select choice's description: the provider kind serving the
/// model, then the card's facts — pricing and context window — when
/// the card knows them. The same line the TUI's picker renders; a
/// client shows it beside the choice's name.
pub(crate) fn card_description(provider: &str, card: &rhema_provider::ModelCard) -> String {
    let mut parts = vec![provider.to_owned()];
    if let Some(pricing) = card.pricing {
        parts.push(format!(
            "${:.2}/M in · ${:.2}/M out",
            pricing.input_per_mtok, pricing.output_per_mtok
        ));
    }
    if let Some(limit) = card.input_token_limit {
        parts.push(format!("{} ctx", format_tokens(limit)));
    }
    parts.join(" · ")
}

/// Humanizes a token count the way the TUI's picker does: 1_048_576 →
/// "1.0M", 131_072 → "131k", small counts stay raw.
pub(crate) fn format_tokens(limit: u64) -> String {
    if limit >= 1_000_000 {
        format!("{:.1}M", limit as f64 / 1_000_000.0)
    } else if limit >= 10_000 {
        format!("{}k", limit / 1_000)
    } else {
        format!("{limit}")
    }
}

/// A driver failure on the wire: an internal error carrying the
/// driver's own text.
pub(crate) fn err_to_error(err: &CodingAgentError) -> Error {
    Error::internal_error().data(err.to_string())
}

/// The engine's stop reasons on the ACP wire. A completed turn never
/// stops on `ToolUse` (the turn loop resolves those), and the remaining
/// engine reasons all read as a normal end of turn.
pub(crate) fn acp_stop(reason: &EngineStop) -> StopReason {
    match reason {
        EngineStop::MaxTokens => StopReason::MaxTokens,
        EngineStop::EndTurn | EngineStop::StopSequence | EngineStop::Other(_) => {
            StopReason::EndTurn
        }
        EngineStop::ToolUse => StopReason::EndTurn,
    }
}

/// Replays a restored history as session updates, live order, before
/// the `session/load` response: user and assistant text as message
/// chunks, calls with their results as completed or failed tool calls.
pub(crate) fn replay(cx: &ConnectionTo<Client>, session_id: &SessionId, session: &Session) {
    for message in &session.history {
        for content in &message.content {
            let update = match (message.role, content) {
                (Role::User, Content::Text(text)) => Some(SessionUpdate::UserMessageChunk(
                    ContentChunk::new(ContentBlock::Text(TextContent::new(text.clone()))),
                )),
                (Role::Assistant, Content::Text(text)) => Some(SessionUpdate::AgentMessageChunk(
                    ContentChunk::new(ContentBlock::Text(TextContent::new(text.clone()))),
                )),
                (Role::Assistant, Content::ToolCall(call)) => {
                    let title = call
                        .args
                        .get("command")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_owned)
                        .unwrap_or_else(|| call.name.clone());
                    Some(SessionUpdate::ToolCall(
                        ToolCall::new(ToolCallId::new(call.id.clone()), title)
                            .name(call.name.clone())
                            .kind(tool_kind(&call.name)),
                    ))
                }
                (Role::Tool, Content::ToolResult(result)) => {
                    Some(SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                        ToolCallId::new(result.id.clone()),
                        ToolCallUpdateFields::new()
                            .status(if result.is_error {
                                ToolCallStatus::Failed
                            } else {
                                ToolCallStatus::Completed
                            })
                            .content(vec![ToolCallContent::from(ContentBlock::Text(
                                TextContent::new(result.content.clone()),
                            ))]),
                    )))
                }
                _ => None,
            };
            if let Some(update) = update {
                let _ = cx.send_notification(SessionNotification::new(session_id.clone(), update));
            }
        }
    }
}
