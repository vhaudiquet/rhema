//! The built-in `turn` kind: `rhema_core::Agent`'s own unit of work.
//!
//! A turn is the one kind this crate ships, because it is the engine's
//! unit: `Agent::turn`, `Agent::resume_turn`, `Session::history`,
//! `StopReason`. It is not in `rhema-core`, because core must not know
//! what a submission is, and it is not in `rhema-host-agent`, because
//! that crate is one assembly of agents and a turn belongs to any of
//! them.

use rhema_core::{
    Agent, AgentError, CommitError, Content, EventSink, ModelOutput, Role, Session, ToolContext,
    close_orphaned_calls,
};
use serde::{Deserialize, Serialize};

use crate::error::SubmissionError;
use crate::kind::{Kind, RecoverCtx, Recovery};
use crate::record::{KindName, Millis, Outcome, Record, SubmissionId};
use crate::state;
use crate::view::Snapshot;

/// The kind tag of a turn.
pub const NAME: &str = "turn";

/// The request of a turn submission.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Payload {
    /// The prompt, exactly as the caller wrote it.
    pub input: String,
    /// `session.history.len()` when the turn started. The input lands at
    /// this index, and the answer is derived from what follows it.
    ///
    /// `None` until the turn starts; `start` pins it.
    #[serde(default)]
    pub base: Option<u64>,
}

/// The `turn` kind, for registration in a [`crate::Kinds`].
#[derive(Debug, Default, Clone, Copy)]
pub struct Turn;

impl Kind for Turn {
    fn name(&self) -> &'static str {
        NAME
    }

    /// The five ordered rules of the turn kind, first match wins:
    ///
    /// 1. `base` missing, or `len < base` (a compaction shortened the
    ///    history): `Interrupted`.
    /// 2. `len == base`: the input never landed, so `Queued`.
    /// 3. `history[base]` is not the payload's input message:
    ///    `Interrupted`.
    /// 4. the tail is an assistant message with no tool call: `Done`,
    ///    because the answer's commit is the settle.
    /// 5. anything else after the input: `Interrupted`.
    fn recover(&self, ctx: &RecoverCtx<'_>) -> Recovery {
        let Ok(payload) = ctx.payload::<Payload>() else {
            return Recovery::Interrupted;
        };
        let Some(base) = payload.base else {
            return Recovery::Interrupted;
        };
        let Ok(base) = usize::try_from(base) else {
            return Recovery::Interrupted;
        };
        let history = &ctx.session.history;
        if history.len() < base {
            return Recovery::Interrupted;
        }
        if history.len() == base {
            return Recovery::Queued;
        }
        if !is_input(&history[base], &payload.input) {
            return Recovery::Interrupted;
        }
        match history.last() {
            Some(last)
                if last.role == Role::Assistant
                    && !last
                        .content
                        .iter()
                        .any(|content| matches!(content, Content::ToolCall(_))) =>
            {
                Recovery::Done
            }
            _ => Recovery::Interrupted,
        }
    }
}

/// Whether `message` is the user message a turn's input lands as.
fn is_input(message: &rhema_core::Message, input: &str) -> bool {
    message.role == Role::User
        && matches!(message.content.first(), Some(Content::Text(text)) if text == input)
}

/// The `turn` kind tag.
pub fn kind() -> KindName {
    KindName::new(NAME).expect("the built-in tag is a valid kind name")
}

fn encode(payload: &Payload) -> serde_json::Value {
    serde_json::to_value(payload).expect("a turn payload is a JSON object")
}

/// Queue a prompt. One write once the caller saves.
pub fn submit(
    session: &mut Session,
    input: impl Into<String>,
    at: Millis,
) -> Result<Record, SubmissionError> {
    let payload = Payload {
        input: input.into(),
        base: None,
    };
    state::submit(session, kind(), encode(&payload), at)
}

/// Start a queued turn, pinning the history index its input will land
/// at. The caller saves before the work begins.
///
/// [`drive`] and [`resume`] call this themselves. A driver that owns its
/// own engine loop, `rhema-host-agent`'s `HostAgent`, for instance,
/// starts the record here and then runs its turn.
pub fn start(
    session: &mut Session,
    id: &SubmissionId,
    input: &str,
    at: Millis,
) -> Result<Record, SubmissionError> {
    let payload = Payload {
        input: input.to_owned(),
        base: Some(session.history.len() as u64),
    };
    state::start(session, id, at, Some(encode(&payload)))
}

/// Start, turn, settle, saving at each of the three points.
///
/// The order is what makes the crash windows safe:
///
/// 1. `start` with the payload's `base` pinned to `history.len()`,
///    then commit;
/// 2. `agent.turn`, whose own commit hook writes the running record plus
///    the history at every boundary;
/// 3. the result mapped to an `Outcome`, `settle`, then commit.
///
/// The turn kind's rules then read a cut session correctly: no input on
/// disk is `Queued`, an input with no final answer is `Interrupted`, an
/// answer already committed is `Done`.
pub async fn drive(
    agent: &mut Agent,
    session: &mut Session,
    id: &SubmissionId,
    sink: &mut dyn EventSink,
    ctx: &ToolContext,
    commit: &mut dyn FnMut(&Session) -> Result<(), CommitError>,
) -> Result<Record, SubmissionError> {
    let input = payload_of(session, id)?.input;
    start(session, id, &input, crate::now())?;
    commit(session)?;
    let result = agent.turn(input, session, sink, ctx).await;
    finish(session, id, result, commit)
}

/// Continue an interrupted turn: retry, `resume_turn`, settle, saving at
/// each of the three points.
///
/// Orphaned tool calls are closed first, with core's
/// [`close_orphaned_calls`], which needs no tools. A driver that wants
/// replay-safe revival calls `revive_dangling_calls` before this, and
/// then there is nothing left to close — the pattern both of
/// `rhema-subagents`' drivers follow.
pub async fn resume(
    agent: &mut Agent,
    session: &mut Session,
    id: &SubmissionId,
    sink: &mut dyn EventSink,
    ctx: &ToolContext,
    commit: &mut dyn FnMut(&Session) -> Result<(), CommitError>,
) -> Result<Record, SubmissionError> {
    close_orphaned_calls(&mut session.history);
    state::retry(session, id, crate::now())?;
    commit(session)?;
    let result = agent.resume_turn(session, sink, ctx).await;
    finish(session, id, result, commit)
}

fn finish(
    session: &mut Session,
    id: &SubmissionId,
    result: Result<ModelOutput, AgentError>,
    commit: &mut dyn FnMut(&Session) -> Result<(), CommitError>,
) -> Result<Record, SubmissionError> {
    let outcome = match result {
        Ok(_) => Outcome::Done,
        Err(err) => Outcome::Failed {
            error: err.to_string(),
            retryable: err.is_retryable(),
        },
    };
    let record = state::settle(session, id, outcome, crate::now())?;
    commit(session)?;
    Ok(record)
}

fn payload_of(session: &Session, id: &SubmissionId) -> Result<Payload, SubmissionError> {
    let snapshot = crate::snapshot(session)?;
    let record = snapshot
        .get(id)
        .ok_or_else(|| SubmissionError::NotFound(id.clone()))?;
    serde_json::from_value(record.payload.clone()).map_err(|err| SubmissionError::Payload {
        id: id.clone(),
        message: err.to_string(),
    })
}

/// The answer, from history, for a settled turn.
///
/// It reads backwards from the tail for the last assistant message and
/// joins its text blocks, so no copy of the answer lives in the record.
/// `None` when the turn has not finished, when its last assistant
/// message carried no text, or when a compaction shortened the history
/// past the input's index.
pub fn answer(session: &Session, snapshot: &Snapshot, id: &SubmissionId) -> Option<String> {
    let record = snapshot.get(id)?;
    let payload: Payload = serde_json::from_value(record.payload.clone()).ok()?;
    let base = usize::try_from(payload.base?).ok()?;
    // A `base` that no longer points at the input names nothing: a
    // compaction shortened the history, or another turn landed there.
    if !is_input(session.history.get(base)?, &payload.input) {
        return None;
    }
    let tail = session.history.get(base..)?;
    for message in tail.iter().rev() {
        if message.role != Role::Assistant {
            continue;
        }
        let text: String = message
            .content
            .iter()
            .filter_map(|content| match content {
                Content::Text(text) => Some(text.as_str()),
                _ => None,
            })
            .collect();
        return (!text.is_empty()).then_some(text);
    }
    None
}
