//! The `subagent` submission kind: a parent's record of work its child
//! session owns.
//!
//! A subagent starts inside a tool call, and a tool never holds
//! `&mut Session`, so the parent cannot record the start before the
//! work. Two things carry it instead:
//!
//! - the child session writes a descriptor into its own
//!   `driver_state` before the first model call, plus a `turn` record
//!   in its own ledger, so the child can say what it is and how far it
//!   got;
//! - [`Kind::adopt`] reads the store's children of one parent and
//!   rebuilds the parent's records from their descriptors.
//!
//! The parent's record points at the child and copies nothing else. A
//! report is the child's last assistant message, derived on demand, so
//! a child that finished while the parent was down still has one.

use std::sync::Arc;

use rhema_core::{Content, Role, Session};
use rhema_session_store::{SessionId, SessionStore, StoreError};
use rhema_submissions::{
    AdoptCtx, Kind, KindName, Outcome, RecoverCtx, Recovery, Status, snapshot, turn,
};
use serde::{Deserialize, Serialize};

use crate::def::SubagentDef;

/// The kind tag of a subagent.
pub const NAME: &str = "subagent";

/// The marker every child id carries after its parent's id.
const CHILD_MARK: &str = ".sub-";

/// The `driver_state` key a child writes its descriptor under.
pub const DESCRIPTOR_KEY: &str = "subagent";

/// A submission of kind `subagent`: the restart recipe.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Payload {
    /// The child session the work lives in.
    pub child: SessionId,
    /// Which definition it runs.
    pub def: String,
    /// What the parent asked.
    pub prompt: String,
    /// The spelling it resolved, so a restart aims the same way.
    #[serde(default)]
    pub model: Option<String>,
}

/// What a child session says about itself, written before its first
/// model call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Descriptor {
    /// The definition the child runs.
    pub def: String,
    /// What the parent asked.
    pub prompt: String,
    /// The model spelling the start carried.
    #[serde(default)]
    pub model: Option<String>,
    /// The parent session this child belongs to.
    pub parent: SessionId,
}

/// Whether `id` names a subagent's child session.
///
/// The picker uses this to offer conversations and not work: a child is
/// a real session file, and resuming it as a conversation would be a
/// second parent. The mark alone is not the verdict: a client-supplied
/// conversation id may spell itself ".sub-", so the mark must follow a
/// valid id and carry [`child_id`]'s minted tail, the identikit this
/// process can check without asking a store — the same strictness
/// [`is_child_of`] gets from its known parent.
pub fn is_child(id: &SessionId) -> bool {
    let raw = id.as_str();
    let Some(mark) = raw.rfind(CHILD_MARK) else {
        return false;
    };
    // The last mark is the one minted last, so a child of a child still
    // reads as a child: everything before it is a valid id, its parent.
    SessionId::new(&raw[..mark]).is_ok() && is_minted_tail(&raw[mark + CHILD_MARK.len()..])
}

/// The tail [`child_id`] appends after the mark: the clock's stamp
/// `{:016}`, its sequence `{:04}`, and the collision retry's `-{n:02}`
/// when the mint needed one. The two shapes sit in one file; a mint
/// format change changes this test with it.
fn is_minted_tail(tail: &str) -> bool {
    let stamp_of = |digits: &str, width: usize| {
        digits.len() == width && digits.bytes().all(|byte| byte.is_ascii_digit())
    };
    let mut parts = tail.split('-');
    matches!(
        (parts.next(), parts.next(), parts.next(), parts.next()),
        (Some(stamp), Some(seq), retry, None)
            if stamp_of(stamp, 16)
                && stamp_of(seq, 4)
                && retry.is_none_or(|retry| stamp_of(retry, 2))
    )
}

/// Whether `id` is a child of `parent`: the prefix rule [`Kind::adopt`]
/// lists one parent's children with.
pub fn is_child_of(parent: &SessionId, id: &SessionId) -> bool {
    id.as_str()
        .strip_prefix(parent.as_str())
        .is_some_and(|tail| tail.starts_with(CHILD_MARK))
}

/// A child id derived from its parent and the clock, made unique
/// against what the store already holds.
pub fn child_id(store: &dyn SessionStore, parent: &SessionId, at: u64) -> Option<SessionId> {
    // A process-local counter keeps two starts in one millisecond
    // apart; the store check keeps a restarted process apart from the
    // children the last one left. A parent id the 128-byte bound
    // cannot hold stays `None`: every child's id rides its parent, so
    // the caller reports the mint as a failure; the turn lives.
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let mut n = 0u32;
    loop {
        let suffix = if n == 0 {
            format!("{at:016}-{seq:04}")
        } else {
            format!("{at:016}-{seq:04}-{n:02}")
        };
        let raw = format!("{parent}{CHILD_MARK}{suffix}");
        let id = match SessionId::new(raw) {
            Ok(id) => id,
            Err(_) => return None,
        };
        match store.load(&id) {
            Ok(None) => return Some(id),
            Ok(Some(_)) => n = n.saturating_add(1),
            // A store that cannot answer still gets a usable name; the
            // save reports the failure, not the mint.
            Err(_) => return Some(id),
        }
    }
}

/// Writes the descriptor into a child session, keeping every other
/// `driver_state` key.
pub fn write_descriptor(session: &mut Session, descriptor: &Descriptor) {
    if !session.driver_state.is_object() {
        session.driver_state = serde_json::Value::Object(serde_json::Map::new());
    }
    if let Some(object) = session.driver_state.as_object_mut() {
        let value = serde_json::to_value(descriptor).unwrap_or(serde_json::Value::Null);
        object.insert(DESCRIPTOR_KEY.to_owned(), value);
    }
}

/// The descriptor a child session carries, if it is one of ours.
pub fn descriptor(session: &Session) -> Option<Descriptor> {
    serde_json::from_value(session.driver_state.get(DESCRIPTOR_KEY)?.clone()).ok()
}

/// Whether the child's tail is a final assistant message, the same
/// rule the turn kind promotes to `Done`.
pub fn finished(session: &Session) -> bool {
    matches!(
        session.history.last(),
        Some(last)
            if last.role == Role::Assistant
                && !last
                    .content
                    .iter()
                    .any(|content| matches!(content, Content::ToolCall(_)))
    )
}

/// The child's report: its last assistant message's text blocks.
pub fn report(session: &Session) -> Option<String> {
    for message in session.history.iter().rev() {
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
        if !text.trim().is_empty() {
            return Some(text);
        }
    }
    None
}

/// The kind, registered by a frontend that has the store and knows
/// which session is the parent.
pub struct SubagentKind {
    store: Arc<dyn SessionStore>,
    parent: SessionId,
}

impl SubagentKind {
    /// The kind over `store`, listing `parent`'s children.
    pub fn new(store: Arc<dyn SessionStore>, parent: SessionId) -> Self {
        Self { store, parent }
    }

    /// The child session a payload names.
    fn child(&self, payload: &Payload) -> Result<Option<Session>, StoreError> {
        self.store.load(&payload.child)
    }
}

impl Kind for SubagentKind {
    fn name(&self) -> &'static str {
        NAME
    }

    fn recover(&self, ctx: &RecoverCtx<'_>) -> Recovery {
        let Ok(payload) = ctx.payload::<Payload>() else {
            return Recovery::Interrupted;
        };
        let child = match self.child(&payload) {
            Ok(Some(child)) => child,
            // The work's own state is gone: no verdict can make it
            // finish.
            Ok(None) => {
                return Recovery::Failed {
                    error: format!("child session {} is gone", payload.child),
                };
            }
            Err(err) => {
                return Recovery::Failed {
                    error: format!("child session {} cannot be read: {err}", payload.child),
                };
            }
        };
        verdict(&child)
    }

    fn adopt(&self, ctx: &AdoptCtx<'_>) -> Vec<serde_json::Value> {
        let Ok(metas) = self.store.list() else {
            return Vec::new();
        };
        let known: Vec<SessionId> = ctx
            .snapshot
            .iter()
            .filter(|record| record.kind.as_str() == NAME)
            .filter_map(|record| {
                serde_json::from_value::<Payload>(record.payload.clone())
                    .ok()
                    .map(|payload| payload.child)
            })
            .collect();
        metas
            .into_iter()
            .filter(|meta| is_child_of(&self.parent, &meta.id))
            .filter(|meta| !known.contains(&meta.id))
            .filter_map(|meta| {
                let child = self.store.load(&meta.id).ok().flatten()?;
                let descriptor = descriptor(&child)?;
                (descriptor.parent == self.parent).then(|| {
                    serde_json::to_value(Payload {
                        child: meta.id,
                        def: descriptor.def,
                        prompt: descriptor.prompt,
                        model: descriptor.model,
                    })
                    .unwrap_or(serde_json::Value::Null)
                })
            })
            .collect()
    }
}

/// What a child session's own ledger and tail say about its work.
fn verdict(child: &Session) -> Recovery {
    let Ok(ledger) = snapshot(child) else {
        return Recovery::Interrupted;
    };
    let newest = ledger
        .iter()
        .rev()
        .find(|record| record.kind == turn::kind());
    if let Some(record) = newest {
        match record.status {
            Status::Done => return Recovery::Done,
            Status::Queued => return Recovery::Queued,
            Status::Interrupted => return Recovery::Interrupted,
            Status::Running => {
                // The child's process is gone too; its turn kind knows
                // how to read the tail it left.
                return turn::Turn.recover(&RecoverCtx {
                    session: child,
                    record,
                });
            }
            Status::Failed => {
                // A failure is a settle: the child named its error
                // before it died, so the verdict is Failed whatever
                // the failure's class. `retryable` is the in-session
                // ladder's decision, made while the work was live; a
                // resume has no business acting on it. Returning
                // `Interrupted` here once made the ledger auto-retry
                // the child at every load, silently re-running an
                // audit whose only fault was a provider 429. The path
                // back is a fresh request with the same recipe: the
                // recovered record is a tombstone that names the
                // error.
                let error = match &record.outcome {
                    Some(Outcome::Failed { error, .. }) => error.clone(),
                    _ => "the subagent failed".to_owned(),
                };
                return Recovery::Failed { error };
            }
            Status::Cancelled => {
                return Recovery::Failed {
                    error: "the subagent was cancelled".to_owned(),
                };
            }
        }
    }
    // No ledger: an older build's child. Its tail is the only record.
    if finished(child) {
        Recovery::Done
    } else {
        Recovery::Interrupted
    }
}

/// A kind tag for registration, validated once.
pub fn kind() -> KindName {
    KindName::new(NAME).expect("the built-in tag is a valid kind name")
}

/// The definition a child's descriptor names, from `defs`.
pub fn definition<'a>(defs: &'a [SubagentDef], name: &str) -> Option<&'a SubagentDef> {
    defs.iter().find(|def| def.name == name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rhema_session_store::SessionId;

    fn id(raw: &str) -> SessionId {
        SessionId::new(raw).unwrap()
    }

    /// The mint's own shape names a child, the retry suffix included,
    /// and a child of a child still does: the last mark is the one
    /// minted last.
    #[test]
    fn a_minted_id_is_a_child() {
        let parent = id("0000000000000001-00");
        let child = id("0000000000000001-00.sub-0000000000000000-0000");
        assert!(is_child(&child));
        assert!(is_child_of(&parent, &child));
        assert!(is_child(&id(
            "0000000000000001-00.sub-0000000000000000-0000-01"
        )));
        assert!(is_child(&id(
            "0000000000000001-00.sub-0000000000000000-0000.sub-0000000000000001-0002"
        )));
    }

    /// A client conversation id that merely contains ".sub-" is not a
    /// child: the picker must not hide it behind work it never was.
    #[test]
    fn a_lookalike_id_is_not_a_child() {
        assert!(!is_child(&id("report.sub-agent")));
        assert!(!is_child(&id("report.sub-0000000000000000-000")));
        assert!(!is_child(&id("sub-0000000000000000-0000")));
        assert!(!is_child(&id("0000000000000001-00.subagent")));
    }
}
